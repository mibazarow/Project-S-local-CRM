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
