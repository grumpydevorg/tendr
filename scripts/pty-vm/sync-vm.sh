#!/bin/bash
# Copy the VM-side slice-1 scripts to ~/slice1 on the VM (heal.sh root-owned).
cd "$(dirname "$0")" || exit 1
COPYFILE_DISABLE=1 tar --no-xattrs -cf - ptydrive.py lib.sh launch.sh reattach.sh resize.sh halfopen.sh heal.sh smoke.sh nested-ssh.sh halfopen-exe.sh halfopen-loopback.sh vmx sync-vm.sh |
  ./vmx 30 'mkdir -p ~/slice1 && cd ~/slice1 && rm -f ._* && sudo rm -f heal.sh && tar xf - && rm -rf __pycache__ && sudo chown root:root heal.sh && sudo chmod 755 heal.sh && ls'
