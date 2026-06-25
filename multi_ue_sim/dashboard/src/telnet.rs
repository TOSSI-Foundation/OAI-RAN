//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
//
// Minimal telnet client for the OAI nr-uesoftmodem telnet server, used by the
// Page-3 mobility engine to fade a UE's downlink via the channel model.
//
// The lever (verified in openair1/SIMULATION/TOOLS/random_channel.c:2156 and applied
// live in radio/rfsimulator/apply_channelmod.c:222):
//
//     channelmod modify <channelid> ploss <dB>
//
// `ploss` is the total path gain in dB: a more-negative value attenuates more (fades
// the cell). The descriptor that attenuates a given UE's DOWNLINK is the one on the
// UE side named "rfsimu_channel_enB0" (the UE receiving from the gNB). So the mobility
// engine drives the telnet server ON EACH UE PROCESS (one port per UE) and modifies
// that UE's enB channel index.
//
// IMPORTANT: this only has any effect when the UE is launched in rfsim mode with
//   --rfsimulator.options chanmod --telnetsrv --telnetsrv.listenport <port>
// On the plain ZMQ proxy path there is no channel model, so these commands are inert.
// The engine surfaces that state to the user rather than pretending it worked.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// UE telnet port convention, matching the rfsim launch in main.rs (9095 + ue index).
pub fn ue_telnet_port(ue: usize) -> u16 {
    9095 + ue as u16
}

/// Send one telnet command line to 127.0.0.1:<port> and return whatever the server
/// replies within `timeout`. Never blocks longer than ~2*timeout. Connection failure
/// (UE not in rfsim/telnet mode) is returned as an Err the caller can display.
pub fn send(port: u16, cmd: &str, timeout: Duration) -> std::io::Result<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    // Drain any banner/prompt the server emits on connect (best-effort, short).
    let mut scratch = [0u8; 1024];
    let _ = stream.read(&mut scratch);

    stream.write_all(cmd.as_bytes())?;
    if !cmd.ends_with('\n') {
        stream.write_all(b"\n")?;
    }
    stream.flush()?;

    // Read the response until the socket idles out (read_timeout) or closes.
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.push_str(&String::from_utf8_lossy(&buf[..n]));
                if out.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => break, // timeout / would-block -> we have what we have
        }
    }
    Ok(out)
}

/// `channelmod modify <chan_idx> <param> <value>` against the UE's telnet server.
pub fn channelmod_modify(ue: usize, chan_idx: u32, param: &str, value: f64) -> std::io::Result<String> {
    let cmd = format!("channelmod modify {} {} {:.2}", chan_idx, param, value);
    send(ue_telnet_port(ue), &cmd, Duration::from_millis(400))
}

/// `channelmod show current` — lists active channel descriptors and their indices,
/// used to discover the UE's enB (downlink) channel index.
pub fn channelmod_show_current(ue: usize) -> std::io::Result<String> {
    send(ue_telnet_port(ue), "channelmod show current", Duration::from_millis(500))
}

/// Probe whether a UE's telnet server (and therefore the rfsim channel model) is
/// reachable. Used by the mobility engine to tell the user "rfsim/chanmod required".
pub fn telnet_reachable(ue: usize) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], ue_telnet_port(ue)));
    TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok()
}

/// Best-effort discovery of the UE's downlink channel index by scanning
/// `channelmod show current` for a line naming "enB". Falls back to 0 (the legacy
/// shared "rfsimu_channel_enB0" index) when the listing can't be parsed.
pub fn discover_enb_channel_idx(ue: usize) -> Option<u32> {
    let listing = channelmod_show_current(ue).ok()?;
    for line in listing.lines() {
        let low = line.to_ascii_lowercase();
        if low.contains("enb") {
            // Find the first integer token on the line: that is the channel index.
            for tok in line.split(|c: char| !c.is_ascii_digit()) {
                if let Ok(idx) = tok.parse::<u32>() {
                    return Some(idx);
                }
            }
        }
    }
    Some(0)
}
