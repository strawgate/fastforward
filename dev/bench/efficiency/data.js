window.BENCHMARK_DATA = {
  "lastUpdate": 1774894925821,
  "repoUrl": "https://github.com/strawgate/memagent",
  "entries": {
    "Efficiency": [
      {
        "commit": {
          "author": {
            "name": "strawgate",
            "username": "strawgate",
            "email": "williamseaston@gmail.com"
          },
          "committer": {
            "name": "strawgate",
            "username": "strawgate",
            "email": "williamseaston@gmail.com"
          },
          "id": "9f3a39116174933a28f5fb6bef967c7c7c0aa352",
          "message": "fix: Add github-token to benchmark-action steps\n\nThe benchmark-action requires github-token when auto-push is enabled.\nWithout it the step fails with: 'auto-push' is enabled but\n'github-token' is not set.\n\nCo-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>",
          "timestamp": "2026-03-30T18:12:52Z",
          "url": "https://github.com/strawgate/memagent/commit/9f3a39116174933a28f5fb6bef967c7c7c0aa352"
        },
        "date": 1774894925572,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "passthrough/logfwd (binary) ms/M-lines",
            "value": 2000,
            "unit": "ms",
            "extra": "n=2"
          }
        ]
      }
    ]
  }
}