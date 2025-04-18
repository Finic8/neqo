#! /usr/bin/env bash

# This script builds the client and server binaries and runs them in a tmux
# session side-by-side. The client connects to the server and the server
# responds with a simple HTTP response. The client and server are run with
# verbose logging and the qlog output is stored in a temporary directory. The
# script also runs tcpdump to capture the packets exchanged between the client
# and server. The script uses tmux to create a split terminal window to display
# the qlog output and the packet capture.

set -e
tmp=$(mktemp -d)

cargo build --locked --bin neqo-client --bin neqo-server

addr=localhost
port=4433
path=/20000
flags="--verbose --verbose --verbose --qlog-dir $tmp --alpn hq-interop --quic-version 1"
if [ "$(uname -s)" != "Linux" ]; then
        iface=lo0
else
        iface=lo
fi

if [ "$NSS_DIR" ] && [ "$NSS_TARGET" ]; then
        export LD_LIBRARY_PATH="$NSS_DIR/../dist/$NSS_TARGET/lib"
        export DYLD_FALLBACK_LIBRARY_PATH="$LD_LIBRARY_PATH"
fi

client="./target/debug/neqo-client $flags --output-dir $tmp --stats https://$addr:$port$path"
server="SSLKEYLOGFILE=$tmp/test.tlskey ./target/debug/neqo-server $flags $addr:$port"

tcpdump -U -i "$iface" -w "$tmp/test.pcap" host $addr and port $port >/dev/null 2>&1 &
tcpdump_pid=$!
trap 'kill $tcpdump_pid; rm -rf "$tmp"' EXIT

tmux -CC \
        set-option -g default-shell "$(which bash)" \; \
        new-session "$client; kill -USR2 $tcpdump_pid; touch $tmp/done" \; \
        split-window -h "$server" \; \
        split-window -v -f "\
                until [ -e $tmp/done ]; do sleep 1; done; \
                echo $tmp; ls -l $tmp; echo; \
                tshark -r $tmp/test.pcap -o tls.keylog_file:$tmp/test.tlskey" \; \
        set remain-on-exit on
