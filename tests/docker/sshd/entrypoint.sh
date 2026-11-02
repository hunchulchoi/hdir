#!/bin/sh
# Key-only login for user "hdir" with the public key the script generated.
set -eu
install -d -m 700 -o hdir -g hdir /home/hdir/.ssh
install -m 600 -o hdir -g hdir /keys/id.pub /home/hdir/.ssh/authorized_keys
exec /usr/sbin/sshd -D -e -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no
