#!/usr/bin/env bash

set -e

#./target/release/acuity purge-chain -y \
#  --base-path staging/$i \
#  --chain staging

./target/release/acuity \
  --base-path staging/$i \
  --chain staging \
  --port 3034$i --rpc-port 994$i --ws-port 995$i \
  --name AcuityStaging$i --validator --pruning archive --rpc-cors=all \
  --bootnodes /dns/localhost/tcp/30341/p2p/12D3KooWAgtK6BujkEcbHmjZaGp9qPUyAJoYYUoRZX4XaCgasG7u \
    /dns/localhost/tcp/30342/p2p/12D3KooWDfoyUGdzxLpKergUQ6gZ6DMJms4qDarw5YsF3oCjE6s3 \
    /dns/localhost/tcp/30343/p2p/12D3KooWR5ajvWjPMB7ebHL1A1TxbAfycDHUPR5W8HmpsN3QxBgf \
    /dns/localhost/tcp/30344/p2p/12D3KooWKSQsphwV66KqTfaR7iVNgBJqwrqkFFAxwPXsJ5iNZMEN
