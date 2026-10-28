// Local-network preflight diagnostics for the shell and SSH tools.
//
// macOS 15+ gates local network access per app. When the grant is missing —
// or when a TUN/VPN client left scoped reject routes behind — the app's
// children still see a working internet but a fake LAN stack: `ifconfig`
// reports `ether 02:00:00:00:00:00` for every interface, `arp` stays empty
// and a connect to 192.168.x.x fails instantly with EHOSTUNREACH. The tools
// themselves run plain `sh -c` / `ssh` with the inherited environment, so the
// snapshot below is about the host, not about the tool.
//
// The probe is cheap, read-only and only runs when a command actually fails
// with an unreachable error (or when the caller asks for `diagnose: true`).
use serde_json::{json, Value};
#[cfg(any(target_os = "macos", test))]
use std::process::{Command, Stdio};
#[cfg(any(target_os = "macos", test))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", test))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

pub const HINT: &str = "Локальная сеть недоступна из окружения инструмента, хотя интернет может работать. \
Проверьте по порядку: 1) «Системные настройки → Конфиденциальность и безопасность → Локальная сеть» — \
Project-S должен быть в списке и включён (после переустановки приложения разрешение может слетать); \
2) TUN/VPN-клиент (FlClash, Proxifier, Kaspersky) — выключите его полностью и перезагрузите Mac: \
после аварийного завершения он оставляет reject-маршруты на en0. Терминал пользователя в это же время \
может видеть сеть нормально — это ожидаемо, разрешение выдаётся каждому приложению отдельно.";

/// Does this process output look like the kernel refused to reach the host?
pub fn looks_unreachable(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no route to host")
        || lower.contains("network is unreachable")
        || lower.contains("host is down")
        || lower.contains("cannot assign requested address")
}

/// First IPv4 literal in the text — enough to aim `route -n get` at the host
/// the failing command mentioned.
pub fn host_from(text: &str) -> Option<String> {
    let mut current = String::new();
    let mut found = Vec::new();
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' {
            current.push(ch);
            continue;
        }
        if !current.is_empty() {
            found.push(std::mem::take(&mut current));
        }
    }
    found.into_iter().find(|candidate| {
        let parts: Vec<&str> = candidate.split('.').collect();
        parts.len() == 4
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
    })
}

/// Safe to splice into the probe script: hostnames and IPs only.
#[cfg(any(target_os = "macos", test))]
fn valid_probe_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 255
        && !target.starts_with('-')
        && target
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
}

#[cfg(any(target_os = "macos", test))]
fn probe_script(target: Option<&str>) -> String {
    let mut script = String::from(
        "if_mac=$(ifconfig en0 2>/dev/null | awk '/ether/{print $2; exit}'); \
ns_mac=$(networksetup -getmacaddress en0 2>/dev/null | awk '{print $3; exit}'); \
zeroed=$(ifconfig -a 2>/dev/null | grep -c 'ether 02:00:00:00:00:00'); \
reject=$(netstat -rn -f inet 2>/dev/null | grep -c '!'); \
arpn=$(arp -an 2>/dev/null | grep -c ' at '); \
printf 'ifconfigMac=%s\\nnetworksetupMac=%s\\nzeroedMacs=%s\\nrejectRoutes=%s\\narpEntries=%s\\n' \
\"${if_mac:-unknown}\" \"${ns_mac:-unknown}\" \"${zeroed:-0}\" \"${reject:-0}\" \"${arpn:-0}\"\n",
    );
    if let Some(target) = target.filter(|t| valid_probe_target(t)) {
        script.push_str(&format!(
            "echo '---route---'\nroute -n get '{target}' 2>&1 | head -6\n"
        ));
    }
    script
}

/// Run the probe script with a hard deadline. `None` means it did not finish.
#[cfg(any(target_os = "macos", test))]
fn run_probe(script: &str) -> Option<String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_string(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
    reader.join().ok()
}

/// Turn the raw probe output into the fields the tools report.
#[cfg(any(target_os = "macos", test))]
pub fn parse_probe(raw: &str) -> Value {
    let (stats, route) = match raw.split_once("---route---") {
        Some((stats, route)) => (stats, Some(route.trim())),
        None => (raw, None),
    };
    let mut data = serde_json::Map::new();
    for line in stats.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "zeroedMacs" | "rejectRoutes" | "arpEntries" => {
                if let Ok(n) = value.parse::<u64>() {
                    data.insert(key.to_string(), json!(n));
                }
            }
            "ifconfigMac" | "networksetupMac" => {
                data.insert(key.to_string(), json!(value));
            }
            _ => {}
        }
    }
    if let Some(route) = route.filter(|r| !r.is_empty()) {
        data.insert("route".to_string(), json!(route));
    }
    Value::Object(data)
}

/// What the numbers mean, in one stable token plus a human sentence.
///
/// Careful: `02:00:00:00:00:00` in `ifconfig` and reject routes on en0 also
/// show up on a Mac where the LAN works fine (a TUN client sits in front of
/// the stack), so they are a hint, not proof. A populated ARP table is the
/// strongest sign that the stack itself is fine.
fn verdict(data: &Value) -> (&'static str, &'static str) {
    let zeroed = data["zeroedMacs"].as_u64().unwrap_or(0);
    let reject = data["rejectRoutes"].as_u64().unwrap_or(0);
    let arp = data["arpEntries"].as_u64().unwrap_or(0);
    if arp > 0 {
        (
            "stack-looks-ok",
            "ARP-таблица заполнена: сетевой стек работает. Скорее всего, хост выключен, \
не в той подсети или закрыт файрволом.",
        )
    } else if zeroed > 0 && reject > 0 {
        (
            "interception-layer",
            "Интерфейсы показывают заглушку 02:00:00:00:00:00, а на en0 есть reject-маршруты: \
либо доступ к локальной сети закрыт именно для Project-S, либо трафик перехватывает TUN/VPN-клиент. \
Если Терминал в этот же момент видит сеть — разрешение выдано ему, а не приложению.",
        )
    } else if zeroed > 0 {
        (
            "zeroed-interfaces",
            "ifconfig показывает заглушку 02:00:00:00:00:00 у интерфейсов — типичный признак \
закрытого доступа к локальной сети на уровне системы.",
        )
    } else if reject > 0 {
        (
            "reject-routes",
            "На en0 есть reject-маршруты (флаг !): обычно это след аварийно завершённого \
TUN/VPN-клиента. Помогает полное выключение клиента и перезагрузка Mac.",
        )
    } else {
        (
            "no-anomaly",
            "Снимок сети не показывает явной блокировки: хост может быть выключен или закрыт файрволом.",
        )
    }
}

fn finish(data: Value) -> Value {
    let (code, summary) = verdict(&data);
    let mut data = data;
    if let Some(map) = data.as_object_mut() {
        map.insert("verdict".to_string(), json!(code));
        map.insert("summary".to_string(), json!(summary));
        map.insert("hint".to_string(), json!(HINT));
    }
    data
}

/// Compact snapshot of the local network stack. macOS probes the real host;
/// other platforms have no equivalent gate, so they report the platform only.
#[cfg(target_os = "macos")]
pub fn snapshot(target: Option<&str>) -> Value {
    match run_probe(&probe_script(target)) {
        Some(raw) => finish(parse_probe(&raw)),
        None => finish(json!({ "error": "network probe timed out" })),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn snapshot(_target: Option<&str>) -> Value {
    finish(json!({ "platform": std::env::consts::OS }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from the report in DOCS/Q.md: every interface zeroed, five
    // reject routes, ARP empty — the app-level local network denial.
    const DENIED: &str = "ifconfigMac=02:00:00:00:00:00\n\
networksetupMac=fc:b2:14:c4:a1:57\n\
zeroedMacs=15\n\
rejectRoutes=5\n\
arpEntries=0\n\
---route---\n\
   route to: 192.168.1.122\n\
destination: 192.168.1.122\n\
       mask: 255.255.255.255\n";

    #[test]
    fn detects_denied_local_network() {
        let data = finish(parse_probe(DENIED));
        assert_eq!(data["verdict"], "interception-layer");
        assert_eq!(data["zeroedMacs"], 15);
        assert_eq!(data["rejectRoutes"], 5);
        assert_eq!(data["arpEntries"], 0);
        assert_eq!(data["ifconfigMac"], "02:00:00:00:00:00");
        assert_eq!(data["networksetupMac"], "fc:b2:14:c4:a1:57");
        assert!(data["route"].as_str().unwrap().contains("192.168.1.122"));
        assert!(data["hint"].as_str().unwrap().contains("Локальная сеть"));
    }

    #[test]
    fn detects_leftover_reject_routes() {
        let raw = "ifconfigMac=fc:b2:14:c4:a1:57\nnetworksetupMac=fc:b2:14:c4:a1:57\n\
zeroedMacs=0\nrejectRoutes=3\narpEntries=0\n";
        let data = finish(parse_probe(raw));
        assert_eq!(data["verdict"], "reject-routes");
    }

    #[test]
    fn populated_arp_outranks_the_zeroed_interface_hint() {
        // A working LAN on a Mac behind a TUN client still shows zeroed MACs
        // and reject routes; a filled ARP table is the stronger signal.
        let working = finish(parse_probe(
            "zeroedMacs=15\nrejectRoutes=6\narpEntries=2\n",
        ));
        assert_eq!(working["verdict"], "stack-looks-ok");
        let clean = finish(parse_probe("zeroedMacs=0\nrejectRoutes=0\narpEntries=0\n"));
        assert_eq!(clean["verdict"], "no-anomaly");
        let zeroed = finish(parse_probe("zeroedMacs=4\nrejectRoutes=0\narpEntries=0\n"));
        assert_eq!(zeroed["verdict"], "zeroed-interfaces");
    }

    #[test]
    fn unreachable_errors_are_recognised() {
        assert!(looks_unreachable("ssh: connect to host 192.168.1.122 port 22: No route to host"));
        assert!(looks_unreachable("nc: connectx to 192.168.1.1 port 22 failed: Network is unreachable"));
        assert!(looks_unreachable("ping: sendto: Host is down"));
        assert!(!looks_unreachable("Permission denied (publickey)."));
        assert!(!looks_unreachable("Connection refused"));
    }

    #[test]
    fn host_is_extracted_from_stderr() {
        assert_eq!(
            host_from("ssh: connect to host 192.168.1.122 port 22: No route to host").as_deref(),
            Some("192.168.1.122")
        );
        assert_eq!(host_from("version 1.2.10.300 is out"), None);
        assert_eq!(host_from("no addresses here"), None);
    }

    #[test]
    fn probe_target_is_sanitised() {
        assert!(valid_probe_target("192.168.1.122"));
        assert!(valid_probe_target("llm.example.ru"));
        assert!(!valid_probe_target("-oProxyCommand=sh"));
        assert!(!valid_probe_target("host; rm -rf /"));
        let script = probe_script(Some("192.168.1.122"));
        assert!(script.contains("route -n get '192.168.1.122'"));
        assert!(!probe_script(Some("-oProxyCommand=sh")).contains("route -n get"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_probe_returns_a_verdict() {
        let data = snapshot(Some("192.0.2.1"));
        assert!(data["verdict"].is_string(), "{data}");
        assert!(data["summary"].is_string());
        assert!(data.get("zeroedMacs").is_some() || data.get("error").is_some());
    }
}
