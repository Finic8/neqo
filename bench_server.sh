#!/usr/bin/bash

export RUST_BACKTRACE=1
#export RUST_LOG=info
export RUST_LOG=info
export QLOGDIR=zlog

export PREVIOUS_RTT=600
export PREVIOUS_CWND_BYTES=3750000
# values from ana
#export PREVIOUS_CWND_BYTES=750000

#sudo ip netns exec ns4s0f1 \
#  ./target/debug/quiche-server \
cargo run --bin neqo-server -- \
  --qns-test 'http3' \
  --qlog-dir $QLOGDIR \
  --cc 'cubic' \
  \
  '0.0.0.0:4433'

# --no-pacing

#  --max-data \
#  10000000000 \
#  --max-stream-data \
#  1000000000 \
#  --max-window \
#  10000000000 \
#  --max-stream-window \
#  10000000000 \
#  --max-streams-bidi 500 \
#  --max-streams-uni 500

#  --disable-gso \
#--disable-hystart
