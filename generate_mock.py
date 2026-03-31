import json
import os

os.makedirs('_site/bench/data/runs', exist_ok=True)
os.makedirs('_site/bench/data/rate-runs', exist_ok=True)
os.makedirs('_site/bench/data/criterion-runs', exist_ok=True)

# Generate a mock competitive run
run_id = "test_run_123"
run_data = {
    "id": run_id,
    "date": "2026-03-31T00:00:00Z",
    "commit": "abcdef",
    "agents": ["logfwd", "vector"],
    "scenario_names": ["passthrough"],
    "scenarios": {
        "passthrough": {
            "logfwd": {"avg_lps": 1000000, "stddev_ms": 1.2},
            "vector": {"avg_lps": 500000, "stddev_ms": 2.5}
        }
    }
}
with open(f'_site/bench/data/runs/{run_id}.json', 'w') as f:
    json.dump(run_data, f)

# Generate a mock index.json
index_data = {
    "runs": [{"id": run_id, "date": "2026-03-31T00:00:00Z"}]
}
with open('_site/bench/data/index.json', 'w') as f:
    json.dump(index_data, f)

print("Mock data generated.")
