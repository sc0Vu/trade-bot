# Monitoring stack

Prometheus, Alertmanager and Grafana for a running bot.

1. Turn on metrics in `config/config.toml`:
   ```toml
   [metrics]
   enabled = true
   # include_money = true   # positions and P&L on the dashboard
   ```
2. Start the stack and the bot:
   ```sh
   docker compose -f deploy/docker-compose.yml up -d
   make run-paper
   ```
3. Open Grafana at http://localhost:3000 (admin / admin on first login);
   the **trade-bot** dashboard is provisioned. Prometheus is at
   http://localhost:9090, with alerts under *Alerts*.

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

Networking: the bot binds `127.0.0.1:9184`. The compose Prometheus reaches
it as `host.docker.internal`, which Docker Desktop maps to the host's
loopback. On Linux, run Prometheus on the host, or bind the bot to the
docker bridge address (`metrics.listen`, with `metrics.allow_public`).
All stack ports bind to `127.0.0.1`.

Checking the configuration (needs `promtool` and `amtool`):
```sh
promtool check rules deploy/prometheus/alerts.yml
promtool test rules deploy/prometheus/alerts_test.yml
amtool check-config deploy/alertmanager/alertmanager.yml
```
