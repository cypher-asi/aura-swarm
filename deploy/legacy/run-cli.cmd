@echo off
setlocal

set "AURA_SWARM_GATEWAY=http://ab6d2375031e74ce1976fdf62ea951a4-e757483aaffba396.elb.us-east-2.amazonaws.com"

echo Using gateway: %AURA_SWARM_GATEWAY%
cargo run -p aura-swarm-cli -- --gateway "%AURA_SWARM_GATEWAY%" %*
