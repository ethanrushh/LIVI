// SPDX-FileCopyrightText: 2026 Ethan Rushbrook
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::AIRPLAY_SERVICE;

const SETTLE: Duration = Duration::from_secs(3);
const CHECK_EVERY: Duration = Duration::from_secs(30);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(2);

pub(super) struct Publisher {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Publisher {
    pub fn start_kill(&mut self) -> io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        Ok(())
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        (&mut self.task).await.map_err(io::Error::other)
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        let _ = self.start_kill();
    }
}

pub(super) fn spawn(name: &str, port: u16, txt: &[String]) -> io::Result<Publisher> {
    let child = process(name, port, txt)?;
    let (stop, stopped) = oneshot::channel();
    let (name, txt) = (name.to_string(), txt.to_vec());
    let task = tokio::spawn(async move {
        tokio::select! {
            () = supervise(child, &name, port, &txt) => {},
            _ = stopped => {},
        }
    });
    Ok(Publisher {
        stop: Some(stop),
        task,
    })
}

fn process(name: &str, port: u16, txt: &[String]) -> io::Result<Child> {
    Command::new(livi_base::sys::tool("avahi-publish-service"))
        .args(["--no-fail", name, AIRPLAY_SERVICE, &port.to_string()])
        .args(txt)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
}

async fn supervise(mut child: Child, name: &str, port: u16, txt: &[String]) {
    let mut delay = SETTLE;
    loop {
        tokio::select! {
            status = child.wait() => {
                eprintln!("[mdns] {name:?} publisher ended: {status:?}");
            },
            () = tokio::time::sleep(delay) => {
                match resolves(port, txt).await {
                    Ok(true) => {
                        delay = CHECK_EVERY;
                        continue;
                    },
                    Ok(false) => eprintln!("[mdns] {name:?} :{port} does not resolve; publishing again"),
                    Err(e) => eprintln!("[mdns] cannot verify {name:?} :{port}: {e}; publishing again"),
                }
            },
        }
        let _ = child.kill().await;
        loop {
            tokio::time::sleep(RETRY).await;
            match process(name, port, txt) {
                Ok(next) => {
                    child = next;
                    delay = SETTLE;
                    break;
                }
                Err(e) => eprintln!("[mdns] cannot publish {name:?} :{port}: {e}"),
            }
        }
    }
}

async fn resolves(port: u16, txt: &[String]) -> io::Result<bool> {
    let device_id = txt
        .iter()
        .find(|s| s.starts_with("deviceid="))
        .ok_or_else(|| io::Error::other("no receiver deviceid"))?;
    let mut browser = Command::new(livi_base::sys::tool("avahi-browse"))
        .args(["-r", "-p", "-k", AIRPLAY_SERVICE])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(browser.stdout.take().expect("piped stdout")).lines();
    let found = tokio::time::timeout(RESOLVE_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            if is_receiver(&line, port, device_id) {
                return Ok::<_, io::Error>(true);
            }
        }
        Ok(false)
    })
    .await
    .unwrap_or(Ok(false));
    let _ = browser.kill().await;
    found
}

fn is_receiver(line: &str, port: u16, device_id: &str) -> bool {
    let fields: Vec<_> = line.splitn(10, ';').collect();
    fields.len() == 10
        && fields[0] == "="
        && fields[4] == AIRPLAY_SERVICE
        && fields[8].parse::<u16>().ok() == Some(port)
        && fields[9].split('"').any(|txt| txt == device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_requires_a_resolved_record_for_this_receiver_and_port() {
        let record = "=;wlan0;IPv4;CarPi;_airplay._tcp;local;CarPi.local;10.10.0.1;43891;\"deviceid=88:A2:9E:56:2B:3D\" \"flags=0x4\"";
        let id = "deviceid=88:A2:9E:56:2B:3D";
        assert!(is_receiver(record, 43891, id));
        assert!(!is_receiver(&record.replacen('=', "+", 1), 43891, id));
        assert!(!is_receiver(record, 54113, id));
        assert!(!is_receiver(record, 43891, "deviceid=88:A2:9E:56:2B:3C"));
        assert!(!is_receiver(
            "+;wlan0;IPv4;CarPi;_airplay._tcp;local",
            43891,
            id
        ));
        assert!(is_receiver(
            &record.replace("CarPi;", "Car\\032Pi;"),
            43891,
            id
        ));
    }
}
