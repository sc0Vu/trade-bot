# Running trade-bot on a VPS

This guide puts the bot on a rented Linux server (DigitalOcean, Hetzner,
Vultr, Linode/Akamai, AWS, Google Cloud, Oracle, or any provider) as a
service. The service starts at boot, restarts after a failure, and keeps
its books on the server's disk.

**You add your API keys yourself, after logging in.** No setup step, script
or form here ever asks for them, and they never leave the server. Paper
trading first: the steps below never touch real money.

## 1. Create the server

- **Image:** Ubuntu 22.04 or newer, or Debian 12 or newer (x86_64 or
  arm64). Other distributions with systemd and glibc 2.35+ work too.
- **Size:** the smallest plan should do: the bot is one process and needs no build
  on the server. Not measured on every plan; watch memory the first days.
- **Region:** Binance refuses connections from US IP addresses (HTTP 451),
  so pick a non-US region for Binance (for example Frankfurt, Amsterdam,
  Singapore). eToro and MAX have their own rules; check them.
- **SSH key:** add your SSH public key when creating the server. Password
  login is not needed.
- **Firewall:** the bot needs no inbound port. If your provider has a
  firewall, allow inbound SSH (22) only. The bot's API and metrics listen
  on 127.0.0.1 unless you change that.
- **Static IP:** if the provider offers one (DigitalOcean *Reserved IP*,
  AWS *Elastic IP*, ...), attach it: you will restrict your API key to
  this address, and a rebuilt server must keep it.

### Setup, automatic (cloud-init)

Most providers have a *user data* / *cloud-init* field on the create
page. Paste [`cloud-init.yaml`](cloud-init.yaml) there. It runs
[`site/vps-setup.sh`](../../site/vps-setup.sh) once when the server
first boots.

On images whose login user isn't `root`, edit `KEYS_FROM=root` in that
file to the image's user (`ubuntu` on AWS Ubuntu, `ec2-user` on Amazon
Linux, `opc` on Oracle), so your SSH key can log in as the bot user.

### Setup, by hand (no cloud-init)

Log in as you normally would on that provider, then:

```sh
curl -fsSL https://sc0vu.github.io/trade-bot/vps-setup.sh -o vps-setup.sh
less vps-setup.sh            # read what it does first
sudo sh vps-setup.sh
```

What the script does, and nothing else:
- creates the user `trade-bot` (no password) and lets the SSH keys you log
  in with log in as it;
- installs the latest release for that user (checksum verified;
  `VERSION=0.2.0` pins one);
- lets that user's services run at boot without a login
  (`loginctl enable-linger`);
- creates `~/bot`, where the bot's books (`data/`) will live.

It opens no port and changes no firewall or SSH setting. It never asks
for keys.

## 2. Log in as the bot user

```sh
ssh trade-bot@<server IP>
cat ~/NEXT-STEPS.txt
```

If the login is refused right after creating the server, setup may still
be running: wait a minute, or as root run `cloud-init status --wait`.

Log in as `trade-bot` directly. `sudo su` / `sudo -iu trade-bot` from
another user also works (the script sets `XDG_RUNTIME_DIR`), but a direct
SSH login is the setup `systemctl --user` expects.

## 3. Create an API key on the exchange

On the exchange's website (Binance testnet or Demo Trading for paper,
your account for live; eToro: its API settings, demo or real):

- **Permissions:** trading only. **Never enable withdrawals.**
- **IP restriction:** this server's IP only. A stolen key then can't be
  used from anywhere else.
- **Network:** keys belong to one network. Testnet, Demo Trading and live
  keys are not interchangeable: a paper run needs testnet or demo keys.

## 4. Add your keys to the server

Write them into a file only you can read, with an editor (so the keys
don't end up in your shell history):

```sh
umask 077
nano ~/.trade-bot.env
```

Binance (spot or USDⓈ-M):

```sh
BINANCE_API_KEY=your-key
BINANCE_API_SECRET=your-secret
```

eToro:

```sh
ETORO_PUBLIC_KEY=your-public-key
ETORO_PRIVATE_KEY=your-user-key
```

MAX paper runs need no key.

Rules for this file: one `NAME=value` per line, no `export`, no spaces
around `=`. Check it's private: `ls -l ~/.trade-bot.env` shows
`-rw-------`. If not: `chmod 600 ~/.trade-bot.env`.

Keys never go in the config file (the bot refuses a config with a key
field) and never in the provider's user data.

## 5. Write the config

```sh
cd ~/bot
trade-bot config init
```

It asks for the strategy, venue, coins, interval and each strategy
parameter, with defaults from the strategy's example, and writes
`config/config.toml`. It writes paper configs only.

Backtest the strategy before trusting it (on your own machine or here):
see the main README's *Backtesting* section.

## 6. Check it against your keys

```sh
trade-bot config check --env-file ~/.trade-bot.env
```

This reports everything `run` would refuse: the config, the live gate,
missing keys (names only, never values), the strategy's parameters. It
connects to nothing, so a wrong key or an unknown symbol shows only when
the bot starts. Fix any `FAIL` line and check again.

## 7. Start it

```sh
cd ~/bot
trade-bot service install --config config/config.toml --env-file ~/.trade-bot.env
trade-bot service status trade-bot-config
journalctl --user -u trade-bot-config -f      # its log; Ctrl-C to stop watching
```

The service is named `trade-bot-<config file name>`. It restarts after a
failure (after reconciling with the exchange), starts at boot, and stops
with a close-out (open orders cancelled) on
`trade-bot service remove trade-bot-config`.

## Day to day

| Task | Command (as `trade-bot`, in `~/bot`) |
|---|---|
| See it | `trade-bot service status trade-bot-config` |
| Log | `journalctl --user -u trade-bot-config -f` |
| Change the config | edit `config/config.toml`, `trade-bot config check --env-file ~/.trade-bot.env`, then `systemctl --user restart trade-bot-config` |
| Rotate a key | create the new key on the exchange, edit `~/.trade-bot.env`, `systemctl --user restart trade-bot-config`, delete the old key on the exchange |
| Update the bot | `curl -fsSL https://sc0vu.github.io/trade-bot/install.sh \| sh`, then `systemctl --user restart trade-bot-config` |
| Stop it | `trade-bot service remove trade-bot-config` (cancels its open orders; positions stay) |

**Back up `~/bot/data/`.** It holds the books: which orders and fills are
the bot's. Lose it while orders are open and the next start refuses (an
unknown `tb…` order on the account) until you start once with
`--cancel-orphans`.

A tripped kill switch stops the service for good (exit code 3, not
restarted). Review what happened, then start once with
`--clear-kill-switch`.

## Real money

Only after a paper run behaved as expected. Three locks, all needed:

1. `mode = "live"` in the config,
2. `exchange.network = "live"`,
3. `--live --i-understand`, typed by you on the install command:

```sh
trade-bot config check --live --env-file ~/.trade-bot.live.env
trade-bot service install --config config/live.toml --env-file ~/.trade-bot.live.env --live --i-understand
```

Use live keys (a separate env file), restricted to this server's IP, with
withdrawals off. The service then trades real money at every start
without asking again.

## Why the keys are handled this way

- **Not in user data:** the provider stores it, and any process on the
  server can read it from the metadata service.
- **Not in the config:** configs get copied, shared and committed. The bot
  refuses a key field in a config.
- **Not typed on a command line:** the shell history keeps it.
- **A file only `trade-bot` can read**, loaded by the service manager into
  the bot's environment at start: the keys exist on that server and
  nowhere else.
