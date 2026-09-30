# Groq free-tier admission probe — September 29, 2026

The current Werd run selected `qwen/qwen3.8-27b`. Its first four requests
started 61 seconds apart; provider-reported total times were 0.34–0.64 seconds.
Input usage grew from 3,720 to 4,806 tokens, with 29–82 output tokens. No cached
usage field appeared and no provider-denial artifacts were present. The long
wait was the local rolling-minute admission rule, not slow inference.

A separate, bounded test used `openai/gpt-oss-120b`, synthetic repeated filler,
a stable system message, low reasoning effort and a 32-token output maximum.
No project content, tools or credentials were printed or changed. Three accepted
requests consumed 15,839 reported tokens total; a fourth request was rejected
before inference. Smaller successful requests were separated by the reported
refill time before attempting a larger request.

| Request | Prompt tokens | Output | HTTP | Observation |
| --- | ---: | ---: | ---: | --- |
| Initial prefix | 4,595 | 18 | 200 | No cached usage reported |
| Extended prefix | 5,595 | 18 | 200 | No cached usage reported |
| Larger extension | 9,127 requested including output reservation | — | 413 | TPM limit 8,000; request too large, despite 8,000 tokens remaining |
| Exact repeat of second request | 5,595 | 18 | 200 | No cached usage reported; remaining allowance fell to 2,387 |

The successful responses also reported 8,000 TPM and 1,000 RPD. The reset
headers imply continuous token refill: for example, 2,387 tokens remaining and
42.097 seconds until full corresponds to replenishing 5,613 tokens at 8,000/min.

These observations establish the account's behavior for this small probe, not
that Groq caching never works. Groq documents GPT-OSS caching, but not Qwen caching
at this date, and says cache refunds happen after processing. We did not observe
usable cache credit or a way to admit a request larger than the account TPM.
Do not enable large-context cache assumptions without evidence from real response
usage and admission behavior.

Implementation follow-up: keep conservative admission and response-header limits,
but refill minute token capacity continuously from persisted charges. Keep request
counts and daily limits as rolling windows. Show `Waiting for Groq budget` for
proactive waits rather than labeling them provider retries. The running project
was not stopped or reconfigured; the new scheduler requires restarting Chuggin.

Sources:
- https://console.groq.com/docs/rate-limits
- https://console.groq.com/docs/prompt-caching
