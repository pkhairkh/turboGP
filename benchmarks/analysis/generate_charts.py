#!/usr/bin/env python3
"""Wave 9: Generate publication-quality charts."""
import csv, math
from pathlib import Path
from collections import defaultdict

REPO = Path("/root/turboGP")
TPCH = REPO / "benchmarks/tpch/results"
CHARTS = REPO / "benchmarks/charts"

def geomean(v):
    v = [x for x in v if x > 0]
    return math.exp(sum(math.log(x) for x in v)/len(v)) if v else 0

def main():
    CHARTS.mkdir(parents=True, exist_ok=True)
    try:
        import matplotlib
        matplotlib.use('Agg')
        import matplotlib.pyplot as plt
        import matplotlib.font_manager as fm
        try:
            fm.fontManager.addfont('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf')
        except: pass
        plt.rcParams['font.sans-serif'] = ['DejaVu Sans']
        plt.rcParams['axes.unicode_minus'] = False
    except ImportError:
        print("matplotlib not available")
        return

    for sf in [1, 10]:
        p = TPCH / f"sf{sf}_results.csv"
        if not p.exists(): continue
        results = list(csv.DictReader(open(p)))
        db_times = defaultdict(list)
        for r in results:
            if r["mode"] == "hot" and r["status"] == "OK":
                db_times[r["database"]].append(int(r["latency_ms"]))
        dbs = ["turbogp", "clickhouse", "duckdb", "postgres"]
        gms = [geomean(db_times.get(d, [0])) for d in dbs]
        fig, ax = plt.subplots(figsize=(10, 6), constrained_layout=True)
        bars = ax.bar(dbs, gms, color=['#e74c3c', '#2ecc71', '#3498db', '#f39c12'])
        ax.set_ylabel('Geomean Latency (ms)')
        ax.set_title(f'TPC-H SF={sf} — Geomean Hot Run Latency (lower is better)')
        for b, v in zip(bars, gms):
            ax.text(b.get_x()+b.get_width()/2, b.get_height()+0.5, f'{v:.0f}ms', ha='center')
        fig.savefig(CHARTS / f"tpch_sf{sf}_geomean.png", dpi=150)
        plt.close(fig)
        print(f"  chart: tpch_sf{sf}_geomean.png")

if __name__ == "__main__":
    main()
