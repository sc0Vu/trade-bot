//! How a run stops on a signal.
//!
//! Ctrl-C (SIGINT), SIGTERM (`kill`, `docker stop`, systemd) and SIGHUP
//! (terminal closed) all mean the same thing: stop trading, then close out,
//! cancelling the bot's open orders so none is left working with no
//! strategy watching it. A second signal during the close-out abandons it,
//! so a person can always get the process back; whatever it left open is
//! found by the next start's reconcile.

use std::future::Future;

/// Resolves on the first shutdown signal, naming it.
pub async fn signal() -> anyhow::Result<&'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut hup = signal(SignalKind::hangup())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r.map(|()| "SIGINT").map_err(Into::into),
            _ = term.recv() => Ok("SIGTERM"),
            _ = hup.recv() => Ok("SIGHUP"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok("Ctrl-C")
    }
}

/// How a close-out ended.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseOut<T> {
    /// It ran to the end with this result.
    Done(T),
    /// A signal arrived first; it was abandoned midway.
    Abandoned(&'static str),
}

/// Runs `close` unless `interrupt` (the next signal) resolves first.
pub async fn close_out_unless<T>(
    close: impl Future<Output = T>,
    interrupt: impl Future<Output = anyhow::Result<&'static str>>,
) -> CloseOut<T> {
    tokio::select! {
        r = close => CloseOut::Done(r),
        s = interrupt => CloseOut::Abandoned(s.unwrap_or("signal")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[tokio::test]
    async fn close_out_finishes_without_a_second_signal() {
        let got = close_out_unless(async { 3 }, pending()).await;
        assert_eq!(got, CloseOut::Done(3));
    }

    /// A close-out stuck on the network must not hold the process hostage.
    #[tokio::test]
    async fn second_signal_abandons_a_stuck_close_out() {
        let got = close_out_unless(pending::<()>(), async { Ok("SIGINT") }).await;
        assert_eq!(got, CloseOut::Abandoned("SIGINT"));
    }
}
