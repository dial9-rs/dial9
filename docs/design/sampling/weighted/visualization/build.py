"""Assemble a single offline artifact, preserving readable simulator sources."""
from pathlib import Path

ROOT = Path(__file__).resolve().parent
source = (ROOT / "page.html").read_text()
simulator = (ROOT / "simulator.js").read_text()
assert "</script" not in simulator.lower()
assert source.count("<!-- SIMULATOR -->") == 1
(ROOT / "sampling-fleet-laboratory.html").write_text(
    source.replace("<!-- SIMULATOR -->", "<script>\n" + simulator + "\n</script>")
)
print(ROOT / "sampling-fleet-laboratory.html")
