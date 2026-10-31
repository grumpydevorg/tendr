#!/bin/bash
# Runs as root on the disposable VM: provision-root.sh '<ssh public key>'.
# Creates the PAM/logind SSH path (sshd-accept on 127.0.0.1:2222) and the test
# users acc and intruder. Its last line is the known_hosts.acc entry for
# halfopen-loopback.sh on the Mac.
set -euxo pipefail
PUBKEY="$1"

for u in acc intruder; do
  id "$u" >/dev/null 2>&1 || useradd --create-home --shell /bin/bash "$u"
  install -d -m 0700 -o "$u" -g "$u" "/home/$u/.ssh"
  printf '%s\n' "$PUBKEY" > "/home/$u/.ssh/authorized_keys"
  chown "$u:$u" "/home/$u/.ssh/authorized_keys"
  chmod 0600 "/home/$u/.ssh/authorized_keys"
  loginctl disable-linger "$u" || true
done

ssh-keygen -A
cat > /etc/ssh/sshd_accept_config <<'EOF'
Port 2222
ListenAddress 127.0.0.1
HostKey /etc/ssh/ssh_host_ed25519_key
UsePAM yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
PermitRootLogin no
AllowUsers acc intruder
PrintMotd no
AcceptEnv LANG LC_*
Subsystem sftp /usr/lib/openssh/sftp-server
EOF
# exeuntu does not run the stock sshd, so its privilege separation directory
# does not exist until something creates it.
install -d -m 0755 /run/sshd
/usr/sbin/sshd -t -f /etc/ssh/sshd_accept_config

cat > /etc/systemd/system/sshd-accept.service <<'EOF'
[Unit]
Description=OpenSSH with PAM/logind on 127.0.0.1:2222 (tendr acceptance)
After=network.target

[Service]
ExecStart=/usr/sbin/sshd -D -e -f /etc/ssh/sshd_accept_config
RuntimeDirectory=sshd
RuntimeDirectoryPreserve=yes
KillMode=process
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now sshd-accept.service
systemctl is-active sshd-accept.service
loginctl show-user acc -p Linger 2>&1 || true
ls /var/lib/systemd/linger 2>/dev/null || true
printf 'slice1-acc %s\n' "$(cut -d' ' -f1,2 /etc/ssh/ssh_host_ed25519_key.pub)"
