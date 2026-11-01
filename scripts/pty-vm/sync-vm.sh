#!/bin/bash
# Copy the scripts to ~/slice1 on the VM and make heal.sh root-owned. Needs
# passwordless sudo on the VM.
cd "$(dirname "$0")" || exit 1
COPYFILE_DISABLE=1 tar --no-xattrs -cf - provision-root.sh install-claude.sh install-helpers.sh ptydrive.py lib.sh launch.sh reattach.sh resize.sh halfopen.sh heal.sh smoke.sh nested-ssh.sh halfopen-exe.sh halfopen-loopback.sh rep_exit.py treekill/treekill.py treekill/reap.py treekill/devserver.py treekill/devwrap.sh treekill/run-shell.sh treekill/run-agent.sh vmx sync-vm.sh |
  ./vmx 30 'mkdir -p ~/slice1 && cd ~/slice1 && rm -f ._* && sudo rm -f heal.sh && tar xf - && rm -rf __pycache__ && sudo chown root:root heal.sh && sudo chmod 755 heal.sh && ls'
