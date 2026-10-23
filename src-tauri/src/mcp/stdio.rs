// stdio MCP transport: spawn the server process with TMPDIR/TEMP/TMP
// re-pointed to Harness/tmp (SPEC 5.2 "the system /tmp is never used") and
// speak newline-delimited JSON-RPC over its pipes. stdout is drained by a
// reader thread so recv() can enforce a timeout; stderr is dropped (it may
// carry unmasked secrets). Dropping the transport kills the child.
use super::jsonrpc::{decode_line, encode_line, McpTransport};
use crate::error::{AppError, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct StdioTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String>>,
    reader: Option<JoinHandle<()>>,
}

impl StdioTransport {
    pub fn spawn(
        command: &str,
        args: &[String],
        extra_env: &BTreeMap<String, String>,
        tmp_dir: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(tmp_dir)?;
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("TMPDIR", tmp_dir)
            .env("TEMP", tmp_dir)
            .env("TMP", tmp_dir);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Io("child stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Io("child stdout missing".into()))?;
        let (tx, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = buf.trim_end_matches(['\r', '\n']);
                        if line.is_empty() {
                            continue;
                        }
                        if tx.send(Ok(line.to_string())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(AppError::Io(e.to_string())));
                        break;
                    }
                }
            }
        });
        Ok(StdioTransport {
            child,
            stdin: Some(stdin),
            lines,
            reader: Some(reader),
        })
    }
}

impl McpTransport for StdioTransport {
    fn send(&mut self, msg: &Value) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(AppError::Io("mcp server pipe is closed".into()));
        };
        stdin.write_all(encode_line(msg)?.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<Value> {
        match self.lines.recv_timeout(timeout) {
            Ok(Ok(line)) => decode_line(&line),
            Ok(Err(e)) => Err(e),
            Err(RecvTimeoutError::Timeout) => Err(AppError::Http(format!(
                "mcp server did not answer within {}s",
                timeout.as_secs()
            ))),
            Err(RecvTimeoutError::Disconnected) => {
                Err(AppError::Io("mcp server closed its output pipe".into()))
            }
        }
    }

    fn close(&mut self) {
        // Closing the write side lets well-behaved servers exit on their own.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.close();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::jsonrpc::RpcChannel;
    use crate::onboarding::temp_test_dir;
    use crate::paths;
    use serde_json::json;

    fn spawn_sh(script: &str, tmp: &Path) -> StdioTransport {
        StdioTransport::spawn(
            "sh",
            &["-c".to_string(), script.to_string()],
            &BTreeMap::new(),
            tmp,
        )
        .unwrap()
    }

    #[test]
    fn stdio_round_trips_a_line_and_skips_blank_output_lines() {
        let wd = temp_test_dir("stdio-roundtrip");
        let tmp = wd.join("tmp");
        // The child answers the single request line it reads, padded with two
        // blank lines the reader thread must drop before decoding.
        let mut transport =
            spawn_sh(r#"read req; printf '\n\n%s\n' "$req""#, &tmp);
        let msg = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": { "nested": { "deep": [1, 2] } }
        });
        transport.send(&msg).unwrap();
        assert_eq!(transport.recv(Duration::from_secs(10)).unwrap(), msg);
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn stdio_recv_times_out_while_child_stays_silent() {
        let wd = temp_test_dir("stdio-timeout");
        let mut transport = spawn_sh("sleep 5", &wd.join("tmp"));
        let err = transport.recv(Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("did not answer within"), "{err}");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn stdio_recv_reports_closed_pipe_after_child_exits() {
        let wd = temp_test_dir("stdio-exit");
        let mut transport = spawn_sh("exit 0", &wd.join("tmp"));
        // The reader thread sees EOF once the child is gone: the channel
        // disconnects and recv surfaces it as an error, never a hang.
        let err = transport.recv(Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("closed its output pipe"), "{err}");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn stdio_send_after_close_reports_closed_pipe() {
        let wd = temp_test_dir("stdio-closed");
        let mut transport = spawn_sh("cat", &wd.join("tmp"));
        transport.close();
        let err = transport.send(&json!({ "jsonrpc": "2.0" })).unwrap_err();
        assert!(err.to_string().contains("pipe is closed"), "{err}");
        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[test]
    fn stdio_child_runs_with_tmp_pointed_at_harness_tmp() {
        let wd = temp_test_dir("mcp-tmp-env");
        let tmp = paths::harness_tmp_dir(&wd);
        // The script waits for the initialize request, then answers with the
        // temp env vars and a configured custom var it received.
        let script = r#"read request; printf '{"jsonrpc":"2.0","id":1,"result":{"tmpdir":"%s","temp":"%s","tmp":"%s","custom":"%s"}}\n' "$TMPDIR" "$TEMP" "$TMP" "$MY_SECRET""#;
        let env = BTreeMap::from([("MY_SECRET".to_string(), "s3cret-value".to_string())]);
        let transport = StdioTransport::spawn("sh", &["-c".to_string(), script.to_string()], &env, &tmp)
            .unwrap();
        let mut channel = RpcChannel::new(Box::new(transport));
        let result = channel
            .request("initialize", json!({}), Duration::from_secs(15))
            .unwrap();
        let want = tmp.display().to_string();
        assert_eq!(result["tmpdir"], want);
        assert_eq!(result["temp"], want);
        assert_eq!(result["tmp"], want);
        assert_eq!(result["custom"], "s3cret-value");
        assert!(tmp.exists());
        std::fs::remove_dir_all(&wd).unwrap();
    }
}
