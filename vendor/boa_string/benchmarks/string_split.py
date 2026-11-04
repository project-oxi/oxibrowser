#!/usr/bin/env python3
"""Manual OxiBrowser String.split timing; requires a binary and UTF-8 source file.

Run the same source and repetition count against before/after optimized binaries.
Uses a blank REPL tab, no network or GPU, and never asserts a timing threshold.
"""
import argparse
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("source", type=Path)
parser.add_argument("--repetitions", type=int, default=10)
args = parser.parse_args()
if args.repetitions < 1:
    parser.error("repetitions must be positive")
source = args.source.read_text(encoding="utf-8")
expression = """(()=>{
  const source=SOURCE, repetitions=REPETITIONS;
  const results=[];
  for(const [encoding,text] of [['original',source],['forced-UTF16','한\\n'+source]]) {
    let count=0;
    const started=performance.now();
    for(let index=0;index<repetitions;index++)count+=text.split('\\n').length;
    results.push({encoding,codeUnits:text.length,repetitions,parts:count,elapsedMs:performance.now()-started});
  }
  return JSON.stringify(results);
})()""".replace("SOURCE", json.dumps(source)).replace("REPETITIONS", str(args.repetitions))
completed = subprocess.run(
    [str(args.binary.resolve()), "session", "--no-audit"],
    input="new\neval t1 " + expression.replace("\n", " ") + "\nexit\n",
    text=True, capture_output=True, check=True, timeout=120,
)
responses = [json.loads(line) for line in completed.stdout.splitlines()]
response = responses[1]
if not response["ok"]:
    raise RuntimeError(response)
results = json.loads(response["data"]["value"])
expected = source.count("\n") + 1
assert [result["parts"] for result in results] == [
    expected * args.repetitions, (expected + 1) * args.repetitions
]
print(json.dumps({"binary": str(args.binary), "source": str(args.source), "results": results}, indent=2))
