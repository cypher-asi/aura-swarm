# Aura Swarm Deploy TUI - run the staged deploy scripts with a live two-column view.
# Run with: .\run-deploy-tui.ps1
#
# The deploy scripts are bash and need your cloud tooling (bash, aws, kubectl,
# terraform, docker) on PATH. On Windows, run this under WSL so `bash` resolves.
#
# Optional environment overrides:
#   $env:AURA_DEPLOY_DIR   = "deploy"                       # scripts directory
#   $env:AURA_DEPLOY_WATCH = "kubectl get pods -n aura"     # infra-state snapshot command

if (-not (Get-Command bash -ErrorAction SilentlyContinue)) {
    Write-Host "Warning: 'bash' was not found on PATH." -ForegroundColor Yellow
    Write-Host "The deploy scripts are bash; on Windows run this TUI under WSL." -ForegroundColor Gray
}

cargo run --release --bin aswarm-deploy
