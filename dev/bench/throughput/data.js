window.BENCHMARK_DATA = {
  "lastUpdate": 1774895630852,
  "repoUrl": "https://github.com/strawgate/memagent",
  "entries": {
    "Throughput": [
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
        "date": 1774894924294,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "passthrough/logfwd (binary) lines/sec",
            "value": 500000,
            "unit": "lines/sec",
            "extra": "avg=200ms stddev=0ms n=2"
          }
        ]
      },
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
          "id": "ecc68b9d6902e262019da16bc8bfcd762f78c538",
          "message": "fix: Clean up micro benchmark output in issue body\n\nStrip ANSI escape codes and cargo compile output from criterion\nresults. Only keep benchmark result lines (time/throughput) and\nwrap in a markdown code block.\n\nCo-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>",
          "timestamp": "2026-03-30T18:24:31Z",
          "url": "https://github.com/strawgate/memagent/commit/ecc68b9d6902e262019da16bc8bfcd762f78c538"
        },
        "date": 1774895630376,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "passthrough/logfwd (binary) lines/sec",
            "value": 500000,
            "unit": "lines/sec",
            "extra": "avg=200ms stddev=0ms n=2"
          }
        ]
      }
    ]
  }
}