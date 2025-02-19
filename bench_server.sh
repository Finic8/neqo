#!/usr/bin/bash

export RUST_BACKTRACE=1
#export RUST_LOG=info
export RUST_LOG=info
export QLOGDIR=../zlog

#sudo ip netns exec ns4s0f1 \
#  ./target/debug/quiche-server \
cargo run --bin neqo-server -- \
  --qns-test 'http3' \
  --qlog-dir $QLOGDIR \
  --cc 'cubic' \
  --cr-saved-rtt 600 \
  --cr-saved-cwnd 3750000 \
  \
  '0.0.0.0:4433'

# --no-pacing

#  --max-data \
#  10_000_000_000 \
#  --max-stream-data \
#  1_000_000_000 \
#  --max-window \
#  10_000_000_000 \
#  --max-stream-window \
#  10_000_000_000 \
#  --max-streams-bidi 500 \
#  --max-streams-uni 500

#  --disable-gso \
#--disable-hystart
