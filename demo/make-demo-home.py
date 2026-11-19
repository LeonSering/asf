#!/usr/bin/env python3
"""Create a fake home folder with invented Claude Code sessions for demos.

Usage: python3 demo/make-demo-home.py /tmp/asf-demo

Then run asf against it, so no real sessions show up:

    HOME=/tmp/asf-demo XDG_CONFIG_HOME=/tmp/asf-demo/.config \\
        XDG_CACHE_HOME=/tmp/asf-demo/.cache asf

Two sessions are marked as open in a running Claude Code process. For that,
the script starts two `sleep` processes that end after 15 minutes.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid

# (folder relative to home, branch, name, age in minutes, live status, messages)
# Messages alternate: user, assistant, user, assistant, ...
SESSIONS = [
    (
        "projects/webshop", "feature/checkout", "checkout-flow-refactor", 3, "busy",
        [
            "The checkout page re-renders on every keystroke. Can you find out why?",
            "The `CheckoutForm` component creates a new `useCart()` selector on every render, "
            "so every input change invalidates the whole cart tree.\n\n"
            "I moved the selector out of the component and memoized the totals. "
            "Typing in the address field no longer re-renders the cart.",
            "Nice. Now split the form into address, shipping and payment steps.",
            "Done. Each step is its own component now:\n\n"
            "- `AddressStep` validates on blur\n"
            "- `ShippingStep` loads rates once and keeps them in the cache\n"
            "- `PaymentStep` only mounts the Stripe element when it is visible\n\n"
            "The step state lives in the URL, so the back button works.",
            "Add tests for the step navigation.",
            "I added 9 tests in `checkout.test.tsx`. They cover forward and back navigation, "
            "validation errors blocking the next step, and restoring the step from the URL.",
        ],
    ),
    (
        "projects/webshop", "main", "fix-cart-rounding-bug", 95, None,
        [
            "Customers report totals that are off by one cent. Here is an example order: 3 x 19.99 with 7% tax.",
            "The tax is rounded per line item and then summed, but the invoice service rounds the total. "
            "For 3 x 19.99 that gives 64.17 vs 64.16.\n\n"
            "I switched both to integer cents and round once at the end.",
            "Make sure old orders still show the same total.",
            "Old orders keep their stored totals; only new carts use the new calculation. "
            "I added a migration test with 200 real-looking orders to confirm nothing changes for them.",
        ],
    ),
    (
        "projects/webshop", "feature/webhooks", "add-stripe-webhooks", 60 * 26, None,
        [
            "We need to handle Stripe webhooks for refunds and failed payments.",
            "I added `/api/webhooks/stripe` with signature verification. It handles "
            "`charge.refunded` and `payment_intent.payment_failed` and is idempotent via the event id.",
            "What happens if our server is down when Stripe sends the event?",
            "Stripe retries for up to 3 days with backoff. Because the handler is idempotent, "
            "replays are safe. I also added a nightly job that reconciles refunds, just in case.",
        ],
    ),
    (
        "projects/weather-cli", "main", "weather-cli-caching", 40, "idle",
        [
            "The weather CLI hits the API on every call. Add a cache.",
            "I added a small file cache in `~/.cache/weather-cli`. Responses are cached per "
            "location for 10 minutes, keyed by rounded coordinates.\n\n"
            "`weather --no-cache` skips it, and `weather cache clear` empties it.",
            "Does the cache handle two processes writing at the same time?",
            "It writes to a temp file and renames it into place, so readers never see a half-written "
            "entry. Two writers just race and the last one wins, which is fine for weather data.",
            "Show the cache age in the output.",
            "The footer now says `cached 4 min ago` when the data comes from the cache.",
        ],
    ),
    (
        "projects/weather-cli", "feature/hourly", "add-hourly-forecast", 60 * 30, None,
        [
            "Add an hourly forecast view with a small temperature chart.",
            "`weather --hourly` now prints the next 24 hours with a sparkline:\n\n"
            "    12 13 14 15 16 17 18\n"
            "    ▂▃▅▆▇▆▅  21°C max\n\n"
            "It reuses the same API response, so it costs no extra request.",
            "Can it use colors for rain?",
            "Hours with more than 50% rain chance are shown in blue. Colors are off when the "
            "output is not a terminal or `NO_COLOR` is set.",
        ],
    ),
    (
        "projects/blog", "main", "blog-dark-mode", 60 * 5, None,
        [
            "Add a dark mode to the blog that follows the system setting.",
            "I added CSS variables for all colors and a `prefers-color-scheme` media query. "
            "Code blocks switch to a dark theme too.",
            "Also add a toggle and remember the choice.",
            "The toggle is in the header and stores the choice in `localStorage`. A tiny inline "
            "script sets the theme before the first paint, so there is no flash on load.",
        ],
    ),
    (
        "projects/blog", "astro", "migrate-to-astro", 60 * 50, None,
        [
            "I want to move the blog from Jekyll to Astro. What would break?",
            "Mostly three things: the Liquid templates, the permalink format and the RSS feed. "
            "Posts in Markdown can stay as they are.\n\n"
            "I would keep the old permalinks with a redirect map so existing links keep working.",
            "Ok, do the migration but keep the URLs.",
            "Done. All 48 posts build, the URLs are unchanged, and the build cache brings rebuilds "
            "down from 40s to 3s.",
        ],
    ),
    (
        "dotfiles", "main", "nvim-lsp-setup", 60 * 72, None,
        [
            "Set up LSP in neovim for Rust and TypeScript.",
            "I added `nvim-lspconfig` with rust-analyzer and the TypeScript server, plus "
            "format on save. Keymaps: `gd` definition, `K` hover, `<leader>rn` rename.",
            "rust-analyzer is slow on startup in big projects.",
            "I set `cargo.buildScripts.enable` and `procMacro.enable` to stay on, but limited "
            "`checkOnSave` to the current crate. Startup in your workspace went from 25s to 8s.",
        ],
    ),
    (
        "", None, "plan-conference-talk", 60 * 8, None,
        [
            "Help me outline a 20 minute talk about caching strategies for web apps.",
            "A possible structure:\n\n"
            "1. Why caching is hard (2 min)\n"
            "2. Browser cache and HTTP headers (5 min)\n"
            "3. CDN and edge caching (5 min)\n"
            "4. Application caches and invalidation (6 min)\n"
            "5. Takeaways (2 min)",
            "Make part 4 more concrete with an example.",
            "Use the shop cart as the running example: a stale cart total after a price change. "
            "Show write-through vs. delete-on-write and why the second one is easier to get right.",
        ],
    ),
    (
        "projects/ml-playground/notebooks", "main", "tune-learning-rate", 60 * 100, None,
        [
            "The training loss jumps around a lot after epoch 3.",
            "The learning rate is too high for the later epochs. I added a cosine schedule with "
            "a short warmup; the loss curve is smooth now and the validation accuracy went up 2%.",
        ],
    ),
    (
        "/tmp/scratch", None, "regex-for-log-lines", 60 * 140, None,
        [
            "Write a regex that extracts the request id and duration from these log lines.",
            "This matches both formats in your sample:\n\n"
            "    req=(?<id>[a-f0-9]{8}).*?took=(?<ms>\\d+)ms\n\n"
            "Run it with `rg -o` or in Python with `re.finditer`.",
        ],
    ),
]


def encode(path: str) -> str:
    """Project folder name, as Claude Code builds it."""
    return re.sub(r"[^A-Za-z0-9]", "-", path)


def dumps(obj) -> str:
    # Compact like Claude Code's own files.
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False)


def start_live_process():
    """A short-lived process to stand in for a running Claude Code."""
    p = subprocess.Popen(
        ["sleep", "900"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    with open(f"/proc/{p.pid}/stat") as f:
        stat = f.read()
    start = stat[stat.rfind(")") + 1 :].split()[19]
    return p.pid, start


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    home = os.path.abspath(sys.argv[1])
    if os.path.exists(home):
        # Only remove folders this script created.
        if not os.path.exists(os.path.join(home, ".asf-demo")):
            sys.exit(f"{home} exists and is not a demo home, refusing to touch it")
        shutil.rmtree(home)

    claude = os.path.join(home, ".claude")
    os.makedirs(os.path.join(claude, "sessions"))
    os.makedirs(os.path.join(home, ".config", "asf"))
    open(os.path.join(home, ".asf-demo"), "w").close()
    with open(os.path.join(home, ".config", "asf", "config"), "w") as f:
        f.write("open = claude --resume {id}\nopen_label = open\n")

    now = time.time()
    for i, (folder, branch, name, age_min, live, messages) in enumerate(SESSIONS):
        cwd = folder if folder.startswith("/") else os.path.join(home, folder) if folder else home
        if cwd.startswith(home):
            os.makedirs(cwd, exist_ok=True)
        sid = str(uuid.UUID(int=0x5E55_0000_0000_4000_8000_0000_0000_0000 + i))
        project = os.path.join(claude, "projects", encode(cwd))
        os.makedirs(project, exist_ok=True)

        t0 = now - age_min * 60 - 60 * len(messages)
        lines = []
        for j, text in enumerate(messages):
            stamp = time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(t0 + 60 * j))
            base = {"cwd": cwd, "sessionId": sid, "timestamp": stamp}
            if branch:
                base["gitBranch"] = branch
            if j % 2 == 0:
                entry = {"type": "user", **base, "message": {"role": "user", "content": text}}
            else:
                entry = {
                    "type": "assistant",
                    **base,
                    "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
                }
            lines.append(dumps(entry))
            if j == 0:
                lines.append(dumps({"type": "agent-name", "agentName": name, "sessionId": sid}))

        path = os.path.join(project, f"{sid}.jsonl")
        with open(path, "w") as f:
            f.write("\n".join(lines) + "\n")
        mtime = now - age_min * 60
        os.utime(path, (mtime, mtime))

        if live:
            pid, start = start_live_process()
            status = {"pid": pid, "sessionId": sid, "cwd": cwd, "procStart": start, "status": live}
            with open(os.path.join(claude, "sessions", f"{pid}.json"), "w") as f:
                f.write(dumps(status))

    print(f"Demo home with {len(SESSIONS)} sessions in {home}")


if __name__ == "__main__":
    main()
