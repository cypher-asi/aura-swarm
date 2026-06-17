# AURA Swarm CLI - Connect to Gateway
# Run with: .\run-cli.ps1
#
# Set these environment variables before running:
#   $env:AURA_SWARM_GATEWAY = "http://your-gateway-url"
#   $env:AURA_SWARM_TOKEN = "your-token"

if (-not $env:AURA_SWARM_GATEWAY) {
    Write-Host "Error: AURA_SWARM_GATEWAY not set. Export it before running." -ForegroundColor Red
    Write-Host "Example: `$env:AURA_SWARM_GATEWAY = 'http://localhost:8080'" -ForegroundColor Gray
    exit 1
}

if (-not $env:AURA_SWARM_TOKEN) {
    Write-Host "Error: AURA_SWARM_TOKEN not set. Export it before running." -ForegroundColor Red
    exit 1
}

Write-Host "Connecting to: $env:AURA_SWARM_GATEWAY" -ForegroundColor Cyan
cargo run --release --bin aswarm
