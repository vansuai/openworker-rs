#!/usr/bin/env python3
"""
每天拉取 GitHub 上 AI 相关的热门项目，渲染为 Markdown。

数据源：GitHub Search API (REST)
- 按 created 或 pushed 时间筛最近 7 天内创建的项目
- 主题 / 文本里包含 AI 相关关键词
- 按 stars 倒序，取前 N 条

产物：workspace/briefings/github-ai-trending-YYYY-MM-DD.md
"""
from __future__ import annotations

import json
import os
import sys
import urllib.parse
import urllib.request
from datetime import date, timedelta, datetime, timezone
from pathlib import Path

# ---- 配置 ----
WORKSPACE = Path(os.environ.get("WORKSPACE", Path.cwd()))
OUT_DIR = WORKSPACE / "briefings"
TOP_N = 15
WINDOW_DAYS = 7  # 看最近 7 天内创建的
LANG_BIAS = ("Python", "TypeScript", "Rust", "Go", "C++", "Java")

# AI 相关关键词（用 OR 组合，搜索 topic / 文本 / 仓库名 / 描述）
# GitHub 搜索关键字会被分词匹配，所以用单词形式更稳
AI_KEYWORDS = [
    "ai", "llm", "gpt", "agent", "rag", "transformer",
    "diffusion", "embedding", "inference", "fine-tuning",
    "machine-learning", "deep-learning",
    "openai", "anthropic", "huggingface", "langchain", "llama",
]

# 限定最近一次 push 时间，能反映「最近活跃的热门 AI 项目」
DATE_FIELD = "pushed"

GITHUB_API = "https://api.github.com/search/repositories"


def build_query() -> str:
    since = (date.today() - timedelta(days=WINDOW_DAYS)).isoformat()
    keyword_clause = " OR ".join(AI_KEYWORDS)
    # stars:>50 过滤低质量项目；pushed:>N 限定最近活跃窗口
    return f"({keyword_clause}) stars:>50 {DATE_FIELD}:>{since}"


def fetch_repos(token: str | None) -> list[dict]:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "daily-ai-trending-bot",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"

    params = {
        "q": build_query(),
        "sort": "stars",
        "order": "desc",
        "per_page": str(TOP_N),
    }
    url = f"{GITHUB_API}?{urllib.parse.urlencode(params)}"

    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=20) as resp:
        data = json.loads(resp.read().decode("utf-8"))

    items = data.get("items", [])
    return items


def fmt_int(n: int | None) -> str:
    if n is None:
        return "-"
    if n >= 1000:
        return f"{n/1000:.1f}k"
    return str(n)


def render_markdown(repos: list[dict], today: date) -> str:
    lines: list[str] = []
    lines.append(f"# GitHub AI 热门项目 · {today.isoformat()}")
    lines.append("")
    lines.append(
        f"> 每日 10:00 自动生成 · 数据源：GitHub Search API · "
        f"窗口：最近 {WINDOW_DAYS} 天创建 · 关键词：LLM / Agent / RAG / Diffusion 等"
    )
    lines.append("")
    lines.append(f"共收录 **{len(repos)}** 个项目。")
    lines.append("")
    lines.append("| # | 项目 | 语言 | ⭐ Stars | 🍴 Forks | 简介 |")
    lines.append("|---|------|------|--------:|--------:|------|")
    for i, r in enumerate(repos, 1):
        name = r.get("full_name", "-")
        url = r.get("html_url", "#")
        lang = r.get("language") or "-"
        stars = fmt_int(r.get("stargazers_count"))
        forks = fmt_int(r.get("forks_count"))
        desc = (r.get("description") or "").replace("|", "\\|").strip()
        if not desc:
            desc = "_无描述_"
        lines.append(f"| {i} | [{name}]({url}) | {lang} | {stars} | {forks} | {desc} |")
    lines.append("")
    lines.append("## 重点速览")
    lines.append("")
    for i, r in enumerate(repos[:5], 1):
        name = r.get("full_name", "-")
        url = r.get("html_url", "#")
        stars = fmt_int(r.get("stargazers_count"))
        desc = (r.get("description") or "").strip() or "_无描述_"
        topics = ", ".join(f"`{t}`" for t in (r.get("topics") or [])[:5])
        lines.append(f"{i}. **[{name}]({url})** ⭐ {stars}")
        lines.append(f"   - {desc}")
        if topics:
            lines.append(f"   - Topics: {topics}")
    lines.append("")
    lines.append("---")
    lines.append(
        f"_生成时间：{datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')}_"
    )
    return "\n".join(lines) + "\n"


def main() -> int:
    token = os.environ.get("GITHUB_TOKEN")  # 可选；不传也能用，只是限速 60/h
    today = date.today()
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    out_path = OUT_DIR / f"github-ai-trending-{today.isoformat()}.md"

    try:
        repos = fetch_repos(token)
    except Exception as e:
        # 失败也留一份空报告，方便排查
        msg = f"# GitHub AI 热门项目 · {today.isoformat()}\n\n> ⚠️ 拉取失败：{e}\n"
        out_path.write_text(msg, encoding="utf-8")
        print(f"[WARN] fetch failed: {e}", file=sys.stderr)
        return 1

    md = render_markdown(repos, today)
    out_path.write_text(md, encoding="utf-8")
    print(f"[OK] wrote {out_path} ({len(repos)} repos)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
