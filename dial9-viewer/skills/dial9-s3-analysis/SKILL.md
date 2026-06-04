---
name: dial9-s3-analysis
description: Analyze dial9 Tokio runtime traces stored in S3 buckets. Use when a user provides an S3 bucket containing dial9 traces and wants to understand runtime behavior, diagnose performance issues, or explore what data is available.
---

# dial9 S3 Bucket Trace Analysis

## Overview

This skill guides you through analyzing dial9 trace data stored in S3. The workflow has three phases:
1. **Discovery** — explore the bucket to show what services, hosts, and time ranges are available
2. **Retrieval** — download and decompress trace files
3. **Analysis** — run the analysis toolkit to produce diagnostic reports

## Prerequisites

- AWS CLI configured with read access to the target bucket
- `dial9` CLI installed (`cargo install dial9` or `cargo binstall dial9`)
- Node.js 14+ for running the analysis toolkit

## Phase 1: Discovery

Present the user with what's in the bucket before doing any analysis.

### Discover bucket structure

```bash
# List date range
aws s3 ls s3://BUCKET/ --region REGION

# List services for a given date/hour
aws s3 ls s3://BUCKET/YYYY-MM-DD/HHMM/ --region REGION

# List all unique service/host/runtime combinations
aws s3 ls s3://BUCKET/ --recursive --region REGION \
  | awk '{print $4}' | awk -F'/' '{if (NF>=5) print $3"/"$4"/"$5}' | sort -u
```

### Expected key structure

dial9 S3 uploads follow this layout:

```
{prefix/}{YYYY-MM-DD}/{HHMM}/{service_name}/{hostname}/{boot_id}/{epoch_secs}-{segment_index}.bin.gz
```

| Component | Meaning |
|-----------|---------|
| `prefix` | Optional. Value of `DIAL9_S3_PREFIX` (default: none, keys start at date). |
| `YYYY-MM-DD` | UTC date. |
| `HHMM` | UTC hour+minute bucket (rotation time determines granularity — default 60s means most keys land on the hour). |
| `service_name` | Value of `DIAL9_SERVICE_NAME` or the binary name. |
| `hostname` | Machine hostname (e.g. `ip-10-0-3-249.ec2.internal`). |
| `boot_id` | 4 random alpha chars + PID, generated at process start (e.g. `nygg-1`). Disambiguates restarts on the same host. |
| `epoch_secs-segment_index` | Unix timestamp of segment start + segment sequence number. |

### Present findings to user

After discovery, present:
- Date range available
- Services found
- Number of hosts (grouped by subnet if applicable)
- Approximate data density (quiet vs busy periods — check file sizes)

Ask the user which host/time period they want to investigate, or if they want a fleet-wide overview.

## Phase 2: Retrieval

### Download trace files

```bash
# Single file
aws s3 cp s3://BUCKET/path/to/file.bin.gz /tmp/d9-traces/ --region REGION

# All files for a host in a time window
aws s3 cp s3://BUCKET/YYYY-MM-DD/HHMM/service/host/ /tmp/d9-traces/ \
  --recursive --region REGION
```

### Decompress

`analyze.js` requires decompressed `.bin` files:

```bash
gunzip /tmp/d9-traces/*.gz
```

Note: If writing custom scripts with `parseTrace()` directly, it handles `.bin.gz` files transparently — decompression is only needed for the `analyze.js` CLI.

## Phase 3: Analysis

### Extract the toolkit

```bash
dial9 agents toolkit /tmp/d9-toolkit
```

### Run automated analysis

```bash
# Single file
node /tmp/d9-toolkit/analyze.js /tmp/d9-traces/file.bin

# All files in a directory
node /tmp/d9-toolkit/analyze.js /tmp/d9-traces/

# Large datasets: sample a subset
node /tmp/d9-toolkit/analyze.js /tmp/d9-traces/ --sample 50
```

### Interpret results

The analyzer reports:

| Section | What to look for |
|---------|-----------------|
| **Setup diagnostic** | Missing data sources (scheduling events, CPU profiling) |
| **Worker utilization** | Imbalanced workers, low utilization (underloaded) or >95% (saturated) |
| **Long polls** | Polls >1ms indicate blocking work on the runtime; >10ms is critical |
| **Scheduling delays** | Wake-to-poll latency >1ms means tasks waiting in queue |
| **Poll duration by spawn** | Which code paths are slowest |
| **CPU hotspots** | Where CPU time is actually spent (requires CPU profiling enabled) |
| **Queue depth** | High global queue = workers can't keep up |
| **Kernel scheduling** | High kernel wait = noisy neighbors or CPU contention |

### When to use other skills

After running the automated analysis:
- **dial9-trace-recipes**: Answer specific diagnostic questions (task leaks, blocking calls, wake chains)
- **dial9-red-flags**: Quick automated health check with fix suggestions
- **dial9-runtime**: Understand runtime behavior from first principles
- **dial9-trace-loading**: Parse traces programmatically for custom analysis

```bash
dial9 agents skill dial9-trace-recipes
dial9 agents skill dial9-red-flags
```

## Choosing what to analyze

| Goal | What to pull |
|------|-------------|
| "Is the service healthy?" | One recent file from any host |
| "Something happened at time X" | All files from the relevant HHMM bucket |
| "Compare hosts" | Same time period from multiple hosts |
| "Track down a latency spike" | Files from the specific hour on the affected host |
| "Fleet overview" | One file per host from the same time window |

## Tips

- **File size indicates load**: Quiet periods typically produce ~35-45KB files; busy periods produce 1-5MB+ files per segment
- **Multiple segments per hour**: Under load, trace rotation produces many files per time bucket — analyze them together by pointing `analyze.js` at the directory
- **Boot IDs are per-process**: The 4-char ID (e.g. `nygg`) is generated at process start. After a restart or deploy, the same host gets a new boot_id
- **Epoch in filename**: The leading number in the filename is the Unix timestamp when that segment started — use it to pick the right file for a time window
- **Large time windows**: For fleet-wide analysis across hundreds of files, use `--sample 50` to analyze a representative subset

## Troubleshooting

- **"Access Denied" or "NoSuchBucket"**: Verify credentials with `aws sts get-caller-identity` and check bucket region
- **Empty bucket listings**: Verify date format is YYYY-MM-DD, region is correct, and prefix matches
- **`dial9` not found**: `cargo install dial9` or `cargo binstall dial9`
- **Analysis errors on .gz files**: Decompress first — `analyze.js` requires raw `.bin` input
- **"Unknown frame tag" errors**: Toolkit version is older than the trace format — update dial9 with `cargo install dial9`
