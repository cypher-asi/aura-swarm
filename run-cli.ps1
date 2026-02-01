# Aura Swarm CLI - Connect to AWS Gateway
# Run with: .\run-cli.ps1

$env:AURA_SWARM_GATEWAY = "http://af4f4466da4d54ad699a4646259de90f-76d669ae8cf34823.elb.us-east-2.amazonaws.com"
$env:AURA_SWARM_TOKEN = "test-token:550e8400-e29b-41d4-a716-446655440000:6ba7b810-9dad-11d1-80b4-00c04fd430c8"

Write-Host "Connecting to: $env:AURA_SWARM_GATEWAY" -ForegroundColor Cyan
Write-Host "Using mock token for user: 550e8400-e29b-41d4-a716-446655440000" -ForegroundColor Gray

cargo run --release --bin aswarm
