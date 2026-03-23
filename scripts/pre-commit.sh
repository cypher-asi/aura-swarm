#!/bin/bash
# Pre-commit checks: formatting, linting, and tests.
#
# Install as a git hook:
#   ln -sf ../../scripts/pre-commit.sh .git/hooks/pre-commit
#
# Or run manually before pushing:
#   ./scripts/pre-commit.sh

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

echo "Running pre-commit checks..."
echo ""

echo "1/3  cargo fmt --check"
if ! cargo fmt --all -- --check; then
    echo -e "${RED}Formatting check failed.${NC} Run 'cargo fmt --all' to fix."
    exit 1
fi
echo -e "${GREEN}✓${NC} Formatting OK"
echo ""

echo "2/3  cargo clippy"
if ! cargo clippy --all-targets --all-features -- -D warnings; then
    echo -e "${RED}Clippy found issues.${NC} Fix them before committing."
    exit 1
fi
echo -e "${GREEN}✓${NC} Clippy OK"
echo ""

echo "3/3  cargo test"
if ! cargo test --all --all-features; then
    echo -e "${RED}Tests failed.${NC} Fix them before committing."
    exit 1
fi
echo -e "${GREEN}✓${NC} Tests OK"
echo ""

echo -e "${GREEN}All pre-commit checks passed!${NC}"
