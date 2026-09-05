#!/usr/bin/env bash
# 跑一次 trending 脚本看输出
set -e
cd "$(dirname "$0")/.."
python3 scripts/daily_ai_trending.py
echo "----"
ls -la briefings/ | tail -20
echo "----"
head -40 "briefings/github-ai-trending-$(date +%F).md" || true
