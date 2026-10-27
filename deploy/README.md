# Monitoring stack

Prometheus (metrics), Alertmanager (alerts), Loki with Grafana Alloy (logs)
and Grafana for a running bot.

1. Turn on metrics and the log file in `config/config.toml`:
   ```toml
   [metrics]
   enabled = true
   # include_money = true   # positions and P&L on the dashboard

   [log]
   file = "data/logs/run.jsonl"   # Alloy tails data/logs/*.jsonl
   ```
2. Start the stack and the bot:
   ```sh
   docker compose -f deploy/docker-compose.yml up -d
   make run-paper
   ```
3. Open Grafana at http://localhost:3000 (admin / admin on first login);
   the **trade-bot** dashboard is provisioned. Prometheus is at
   http://localhost:9090, with alerts under *Alerts*.

The dashboard's last panels come from Loki: warnings and errors over
time, and the log lines themselves. Every line of a run carries its run
tag (`span.tag` in the file, `run` in Loki). Engine order ids (`o-1`,
`o-2`, …) restart every run; the tag tells runs apart and is the prefix
of the client order id Binance and the order journal hold, so log `o-2`
is `<tag>-o-2` there. Type a tag into the dashboard's *run tag* box, or
query Loki directly:
```
{job="trade-bot", level="WARN"}
{job="trade-bot"} | run="tbmvqk2hzs" |= "fill"
```
The bot doesn't rotate the file; rotate with `copytruncate` (logrotate
or newsyslog), which Alloy follows.

Alerts reach nobody until you give Alertmanager a receiver: edit
`alertmanager/alertmanager.yml` (email, Slack, Telegram, a webhook).

| Alert | Fires when |
|---|---|
| `TradeBotDown` | `/metrics` unreachable for 1 min |
| `FeedStale` | No closed candle for over 150 s (1m bars; scale for others) |
| `SignalsRejected` | More than 3 refusals of one kind in 15 min |
| `BinanceErrors` | More than 5 errors on one endpoint in 5 min |
| `BinanceSlow` | p99 latency over 2 s for 5 min |
| `FillsRecoveredAtStartup` | Startup reconcile found fills made while the bot was down |
| `TradeBotMemoryHigh` | Resident memory over 500 MB for 10 min |
| `BinanceRateLimited` | Binance answered 429 or 418 in the last 10 min |
| `BinanceWeightHigh` | Over 4000 of the 4800 weight budget for 5 min |
| `RateLimiterWaiting` | The limiter held requests more than 5 times in 15 min |
| `FeedReconnecting` | The market-data feed reconnected more than 3 times in an hour |

Networking: the bot binds `127.0.0.1:9184`. The compose Prometheus reaches
it as `host.docker.internal`, which Docker Desktop maps to the host's
loopback. On Linux, run Prometheus on the host, or bind the bot to the
docker bridge address (`metrics.listen`, with `metrics.allow_public`).
All stack ports bind to `127.0.0.1` (Loki at http://localhost:3100).

## Other backends

Nothing here is Grafana-specific; the bot exposes Prometheus `/metrics`
and writes JSON lines.
- **Grafana Cloud**: point `loki.write` in `alloy/config.alloy` at your
  stack's Loki URL with `basic_auth`, and add a `prometheus.scrape` +
  `prometheus.remote_write` pair for the metrics.
- **Datadog**: the agent's OpenMetrics check scrapes `/metrics`; its log
  collection tails the file (`type: file`, `source: trade-bot`). The JSON
  keys `message` and `level` map onto Datadog's without remapping.
- **Vector, Fluent Bit, ELK, OpenTelemetry Collector**: tail the file as
  JSON lines (`timestamp` is RFC 3339, UTC) and scrape `/metrics`.
- **Containers / systemd**: `[log] format = "json"` puts the same lines
  on stdout for `docker logs`, journald or Kubernetes collectors.

Checking the configuration (needs `promtool`, `amtool`, and Docker for
Loki and Alloy):
```sh
promtool check rules deploy/prometheus/alerts.yml
promtool test rules deploy/prometheus/alerts_test.yml
amtool check-config deploy/alertmanager/alertmanager.yml
docker compose -f deploy/docker-compose.yml run --rm loki -config.file=/etc/loki/loki.yml -verify-config
docker compose -f deploy/docker-compose.yml run --rm alloy fmt /etc/alloy/config.alloy
```
