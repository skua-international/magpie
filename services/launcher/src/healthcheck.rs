//! `/launcher healthcheck` -- a real Source-engine A2S_INFO round trip
//! against arma3server_x64's own Steam query port (its game port + 1,
//! fixed -- nothing in launch.rs's own args makes that configurable,
//! matching what's actually observed: `-port=2302` always yields query
//! port 2303), not just "is the process still alive". A crashed/hung
//! server process still holds its container up (Kubernetes has no signal
//! for "PID 1 exited but something else is still running" the other way
//! around), and a process that's alive but deadlocked mid-init would pass
//! any process-liveness check forever -- this actually confirms the game
//! server is accepting and answering queries, the same thing a real
//! player's server browser does. Exec'd from a probe (see
//! reconcile.rs's own doc) rather than exposed as a TCP endpoint since
//! Kubernetes probes have no native UDP support at all.
use std::net::UdpSocket;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);
const A2S_HEADER: &[u8] = b"\xff\xff\xff\xffTSource Engine Query\x00";

pub fn run() -> anyhow::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2302);
    let query_port = port
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("PORT {port} has no room for a query port"))?;

    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.connect(("127.0.0.1", query_port))?;

    // A2S_INFO is nominally challenge-gated -- the first request can get
    // back an A2S_SERVERQUERY_GETCHALLENGE ('A') reply carrying a
    // challenge to resend instead of the actual info -- but confirmed
    // live this server doesn't consistently require that round trip (two
    // otherwise-identical queries a few minutes apart, one came back 'A',
    // the other answered directly with 'I'). Handle both: only do the
    // second round trip if actually challenged.
    socket.send(A2S_HEADER)?;
    let mut buf = [0u8; 1024];
    let mut n = socket.recv(&mut buf)?;

    if n >= 5 && buf[4] == b'A' {
        let challenge = buf.get(5..9.min(n)).unwrap_or_default().to_vec();
        anyhow::ensure!(
            challenge.len() == 4,
            "truncated challenge in {n}-byte reply"
        );

        let mut request = A2S_HEADER.to_vec();
        request.extend_from_slice(&challenge);
        socket.send(&request)?;
        n = socket.recv(&mut buf)?;
    }

    // 'I' (0x49) is A2S_INFO's own response header -- anything else means
    // this wasn't actually answered as a real info query.
    anyhow::ensure!(
        n >= 5 && buf[4] == b'I',
        "expected an A2S_INFO reply, got {} bytes starting {:?}",
        n,
        &buf[..n.min(8)]
    );

    Ok(())
}
