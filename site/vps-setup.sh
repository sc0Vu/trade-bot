#!/bin/sh
# Prepares a fresh Linux VPS (any provider) to run trade-bot as a service.
#
#   curl -fsSL https://sc0vu.github.io/trade-bot/vps-setup.sh | sudo sh
#
# or paste deploy/vps/cloud-init.yaml (or this file) as the server's user
# data when creating it. Run as root, once; running it again is harmless.
#
# It creates the user `trade-bot` (BOT_USER), lets the SSH keys you created
# the server with log in as it, installs the latest release for that user
# with install.sh (checksum verified; VERSION=0.2.0 pins one), lets its
# services run without a login (loginctl enable-linger), and makes ~/bot,
# the directory the bot's books (data/) live in.
#
# It never asks for, receives or writes API keys: you log in and add them
# yourself (deploy/vps/README.md). It opens no port and changes no firewall
# or SSH setting.
#
# Needs systemd and glibc 2.35+ (Ubuntu 22.04+, Debian 12+, and other
# current distributions), x86_64 or arm64, and curl, tar and sha256sum
# (installed with apt or dnf if missing).
#
# KEYS_FROM: users whose ~/.ssh/authorized_keys may also log in as the bot
# user, comma-separated. Default: root and the user running sudo. Set it on
# images whose login user isn't root (KEYS_FROM=ubuntu on AWS Ubuntu,
# ec2-user on Amazon Linux, opc on Oracle).
set -eu

BOT_USER="${BOT_USER:-trade-bot}"
INSTALL_URL="${INSTALL_URL:-https://sc0vu.github.io/trade-bot/install.sh}"
VERSION="${VERSION:-}"
KEYS_FROM="${KEYS_FROM:-root${SUDO_USER:+,$SUDO_USER}}"

say() { echo "vps-setup: $*"; }
fail() { echo "vps-setup: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || fail "run as root: curl -fsSL $INSTALL_URL | sudo sh"
[ "$(uname -s)" = Linux ] || fail "Linux only"
command -v systemctl >/dev/null 2>&1 || fail "needs systemd (the bot runs as a systemd user service)"
command -v loginctl >/dev/null 2>&1 || fail "needs loginctl (systemd-logind)"
case "$BOT_USER" in
  "" | *[!a-z0-9_-]*) fail "BOT_USER must be lowercase letters, digits, _ or -" ;;
esac
case "$VERSION" in
  *[!0-9.]*) fail "VERSION must look like 0.2.0" ;;
esac

# Release binaries link glibc 2.35+.
glibc=$(ldd --version 2>/dev/null | head -n 1 | sed -n 's/.* \([0-9][0-9]*\.[0-9][0-9]*\)$/\1/p')
if [ -n "$glibc" ]; then
  major=${glibc%%.*}
  minor=${glibc#*.}
  if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; then
    fail "glibc $glibc is too old (2.35+ needed: Ubuntu 22.04+, Debian 12+)"
  fi
else
  fail "no glibc found (musl distributions such as Alpine: use the container image)"
fi

missing=""
for tool in curl tar sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
  say "installing$missing"
  if command -v apt-get >/dev/null 2>&1; then
    DEBIAN_FRONTEND=noninteractive apt-get update -q
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q ca-certificates curl tar coreutils
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y -q ca-certificates curl tar coreutils
  else
    fail "install$missing, then run this again"
  fi
fi

if id "$BOT_USER" >/dev/null 2>&1; then
  say "user $BOT_USER exists"
else
  shell=$(command -v bash || echo /bin/sh)
  useradd --create-home --shell "$shell" "$BOT_USER"
  say "created user $BOT_USER (no password: SSH key login only)"
fi
home=$(getent passwd "$BOT_USER" | cut -d: -f6)
[ -d "$home" ] || fail "$BOT_USER has no home directory"

# The keys the server was created with log in as the bot user too, so its
# login gets a proper systemd user session (systemctl --user works).
umask 077
mkdir -p "$home/.ssh"
touch "$home/.ssh/authorized_keys"
copied=0
old_ifs=$IFS
IFS=,
for from in $KEYS_FROM; do
  from_home=$(getent passwd "$from" | cut -d: -f6 || true)
  keys="$from_home/.ssh/authorized_keys"
  if [ -n "$from_home" ] && [ -s "$keys" ] && [ "$keys" != "$home/.ssh/authorized_keys" ]; then
    # Cloud images prefix root's keys with a "please log in as ubuntu"
    # command; keep only the key itself.
    awk '{ for (i = 1; i < NF; i++) if ($i ~ /^(ssh-|ecdsa-|sk-)/) { print $i, $(i + 1); break } }' \
      "$keys" >>"$home/.ssh/authorized_keys"
    copied=1
  fi
done
IFS=$old_ifs
sort -u -o "$home/.ssh/authorized_keys" "$home/.ssh/authorized_keys"
chown -R "$BOT_USER:" "$home/.ssh"
chmod 700 "$home/.ssh"
chmod 600 "$home/.ssh/authorized_keys"
if [ "$copied" = 0 ] && [ ! -s "$home/.ssh/authorized_keys" ]; then
  say "WARNING: no SSH key found in KEYS_FROM=$KEYS_FROM; add one to $home/.ssh/authorized_keys"
fi
umask 022

say "installing trade-bot for $BOT_USER"
su - "$BOT_USER" -c "curl -fsSL '$INSTALL_URL' | VERSION='$VERSION' sh"

# ~/.local/bin on the PATH; XDG_RUNTIME_DIR for `sudo -iu` (an SSH login
# sets it already) so systemctl --user finds the user's manager.
profile="$home/.profile"
touch "$profile"
if ! grep -q 'trade-bot: PATH' "$profile"; then
  cat >>"$profile" <<'EOF'

# trade-bot: PATH and the user service manager
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) PATH="$HOME/.local/bin:$PATH" ;; esac
export PATH
[ -n "${XDG_RUNTIME_DIR:-}" ] || export XDG_RUNTIME_DIR="/run/user/$(id -u)"
EOF
fi
chown "$BOT_USER:" "$profile"

# Services start at boot, without anyone logged in.
loginctl enable-linger "$BOT_USER"

su - "$BOT_USER" -c 'mkdir -p ~/bot'
cat >"$home/NEXT-STEPS.txt" <<EOF
trade-bot is installed. Nothing runs yet, and no key is on this server.

1. Put your exchange API keys in a file only you can read
   (trade only, no withdrawals, restricted to this server's IP):
     umask 077 && nano ~/.trade-bot.env
   with lines like:
     BINANCE_API_KEY=...
     BINANCE_API_SECRET=...
2. Write a config by answering questions (paper only):
     cd ~/bot && trade-bot config init
3. Check it against those keys:
     trade-bot config check --env-file ~/.trade-bot.env
4. Start it as a service (restarted on failure, started at boot):
     trade-bot service install --config config/config.toml --env-file ~/.trade-bot.env

Full guide: https://github.com/sc0Vu/trade-bot/blob/main/deploy/vps/README.md
EOF
chown "$BOT_USER:" "$home/NEXT-STEPS.txt"

say "done. Log in as $BOT_USER (ssh $BOT_USER@<this server's IP>) and read ~/NEXT-STEPS.txt"
