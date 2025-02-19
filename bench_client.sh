#!/usr/bin/bash

#alias srvnet="sudo ip netns exec ns4s0f1"

export RUST_BACKTRACE=1
export RUST_LOG=info
#export QLOGDIR=log

SHA=7cf08c48959f9a2e2e64bad81e4a4a742f5a8dbc0e590fb93617f22d08782dc8

OUT_DIR=/tmp/neqo-out
FILE=20MB.file
rm -r $OUT_DIR/$FILE

#sudo ip netns exec ns3s0f0 \
#  ./target/debug/quiche-client \
sudo ip netns exec ns_cli sudo -u n \
  cargo run --bin neqo-client -- \
  --output-dir $OUT_DIR \
  \
  https://10.4.0.2:4433/$FILE
#  https://$1/$FILE

#--disable-hystart \
#  --max-data \
#  10000000000 \
#  --max-stream-data \
#  1000000000 \
#  --max-window \
#  10000000000 \
#  --max-stream-window \
#  10000000000 \
#  --max-streams-bidi 500 \
#  --max-streams-uni 500 \

echo $SHA expected
sha256sum $OUT_DIR/$FILE
