#!/usr/bin/env python3
"""Wave 5: Generate ClickBench dataset (100M rows).

ClickBench uses a pre-defined hits table schema. We generate synthetic data
matching the schema at 100M rows (~15 GB CSV).
"""
import csv
import os
import random
import sys
from pathlib import Path

REPO = Path("/root/turboGP")
DATA_DIR = REPO / "benchmarks/clickbench/data"
NUM_ROWS = 100_000_000  # 100M rows
BATCH_SIZE = 1_000_000
OUTPUT_FILE = DATA_DIR / "hits.csv"

# ClickBench hits table schema (from https://github.com/ClickHouse/ClickBench)
# 105 columns — we generate the core subset that the 43 queries reference.
COLUMNS = [
    "WatchID", "JavaEnable", "Title", "GoodEvent", "EventTime", "EventDate",
    "CounterID", "ClientIP", "RegionID", "UserID", "CounterClass", "OS",
    "UserAgent", "URL", "Referer", "IsRefresh", "RefererCategoryID",
    "SendLog", "Age", "Sex", "Income", "Interests", "Robotness", "RemoteIP",
    "WindowName", "OpenerName", "HTTPOrigin", "UserAgentMajor", "UserAgentMinor",
    "Cookie", "IPNetworkID", "SilverlightVersion3", "CodeVersion", "ResolutionWidth",
    "ResolutionHeight", "UserAgentMinor2", "FlashMajor", "FlashMinor",
    "NetMajor", "NetMinor", "MobilePhone", "SilverlightVersion1", "SilverlightVersion2",
    "Hidden", "Final", "EventTimestamp", "IntervalSeconds", "Price",
    "OpenpageService", "WMIID", "ChromeMajor", "ChromeMinor", "BrowserMajor",
    "BrowserMinor", "BrowserEngineID", "OSMajor", "OSMinor", "EngineMajor",
    "EngineMinor", "UserAgentID", "ClientEventTime", "AdvEngineID", "RequestNum",
    "ResolutionString", "BrowserEngine", "Browser", "OSFullName", "AdvEngine",
    "URLDomain", "RefererDomain", "ClientEventDate", "RequestTryNum", "EventYear",
    "EventMonth", "EventDay", "EventHour", "EventMinute", "EventSecond",
    "ResolutionX", "ResolutionY", "FlashVersion", "NetVersion", "SilverlightVersion",
    "UserAgentVersion", "BrowserVersion", "ChromeVersion", "OSVersion", "EngineVersion",
    "MobilePhoneModel", "RobotName", "RegionName", "CountryName", "CityName",
    "BrowserName", "EngineName", "AdvEngineName", "CounterName", "Domain",
    "PageCharset", "TitleLength", "URLLength", "RefererLength", "CounterName2",
    "AdvEngineName2", "EventDate2",
]

def generate_row(row_id):
    """Generate a single synthetic ClickBench row."""
    return [
        row_id,                                          # WatchID (BIGINT)
        random.randint(0, 1),                            # JavaEnable
        f"Title_{row_id % 1000}",                        # Title
        random.randint(0, 1),                            # GoodEvent
        f"2020-01-{1 + row_id % 30:02d} {random.randint(0,23):02d}:{random.randint(0,59):02d}:{random.randint(0,59):02d}",  # EventTime
        f"2020-01-{1 + row_id % 30:02d}",                # EventDate
        random.randint(1, 1000),                         # CounterID
        random.randint(0, 2**31 - 1),                    # ClientIP
        random.randint(1, 10000),                        # RegionID
        random.randint(1, 1000000),                      # UserID
        random.randint(1, 10),                           # CounterClass
        random.randint(1, 100),                          # OS
        random.randint(1, 1000),                         # UserAgent
        f"http://example.com/page_{row_id % 10000}",     # URL
        f"http://referer.com/ref_{row_id % 5000}",       # Referer
        random.randint(0, 1),                            # IsRefresh
        random.randint(1, 100),                          # RefererCategoryID
        random.randint(0, 1),                            # SendLog
        random.randint(0, 100),                          # Age
        random.randint(0, 1),                            # Sex
        random.randint(0, 1000),                         # Income
        random.randint(0, 100),                          # Interests
        random.randint(0, 1),                            # Robotness
        random.randint(0, 2**31 - 1),                    # RemoteIP
        f"win_{row_id % 100}",                           # WindowName
        f"opener_{row_id % 100}",                        # OpenerName
        "https://origin.example.com",                    # HTTPOrigin
        random.randint(1, 100),                          # UserAgentMajor
        random.randint(0, 9),                            # UserAgentMinor
        random.randint(0, 1),                            # Cookie
        random.randint(1, 1000),                         # IPNetworkID
        random.randint(0, 10),                           # SilverlightVersion3
        random.randint(1, 100),                          # CodeVersion
        random.randint(320, 3840),                       # ResolutionWidth
        random.randint(240, 2160),                       # ResolutionHeight
        random.randint(0, 9),                            # UserAgentMinor2
        random.randint(0, 50),                           # FlashMajor
        random.randint(0, 9),                            # FlashMinor
        random.randint(0, 10),                           # NetMajor
        random.randint(0, 9),                            # NetMinor
        random.randint(0, 1),                            # MobilePhone
        random.randint(0, 10),                           # SilverlightVersion1
        random.randint(0, 10),                           # SilverlightVersion2
        random.randint(0, 1),                            # Hidden
        random.randint(0, 1),                            # Final
        random.randint(1577836800, 1580515200),          # EventTimestamp
        random.randint(0, 100),                          # IntervalSeconds
        round(random.uniform(0, 100), 2),                # Price
        random.randint(1, 100),                          # OpenpageService
        f"wmi_{row_id % 1000}",                          # WMIID
        random.randint(0, 100),                          # ChromeMajor
        random.randint(0, 9),                            # ChromeMinor
        random.randint(0, 100),                          # BrowserMajor
        random.randint(0, 9),                            # BrowserMinor
        random.randint(1, 20),                           # BrowserEngineID
        random.randint(1, 20),                           # OSMajor
        random.randint(0, 9),                            # OSMinor
        random.randint(1, 20),                           # EngineMajor
        random.randint(0, 9),                            # EngineMinor
        random.randint(1, 1000),                         # UserAgentID
        f"2020-01-{1 + row_id % 30:02d} {random.randint(0,23):02d}:{random.randint(0,59):02d}:{random.randint(0,59):02d}",  # ClientEventTime
        random.randint(1, 20),                           # AdvEngineID
        random.randint(1, 1000),                         # RequestNum
        f"{random.randint(320,3840)}x{random.randint(240,2160)}",  # ResolutionString
        f"Engine_{row_id % 20}",                         # BrowserEngine
        f"Browser_{row_id % 50}",                        # Browser
        f"OS_{row_id % 20}",                             # OSFullName
        f"AdvEngine_{row_id % 20}",                      # AdvEngine
        "example.com",                                   # URLDomain
        "referer.com",                                   # RefererDomain
        f"2020-01-{1 + row_id % 30:02d}",                # ClientEventDate
        random.randint(1, 10),                           # RequestTryNum
        2020,                                            # EventYear
        1,                                               # EventMonth
        1 + row_id % 30,                                 # EventDay
        row_id % 24,                                     # EventHour
        row_id % 60,                                     # EventMinute
        row_id % 60,                                     # EventSecond
        random.randint(320, 3840),                       # ResolutionX
        random.randint(240, 2160),                       # ResolutionY
        f"{random.randint(0,50)}.{random.randint(0,9)}", # FlashVersion
        f"{random.randint(0,10)}.{random.randint(0,9)}", # NetVersion
        f"{random.randint(0,10)}.{random.randint(0,9)}", # SilverlightVersion
        f"{random.randint(0,100)}.{random.randint(0,9)}", # UserAgentVersion
        f"{random.randint(0,100)}.{random.randint(0,9)}", # BrowserVersion
        f"{random.randint(0,100)}.{random.randint(0,9)}", # ChromeVersion
        f"{random.randint(1,20)}.{random.randint(0,9)}", # OSVersion
        f"{random.randint(1,20)}.{random.randint(0,9)}", # EngineVersion
        f"Phone_{row_id % 50}",                          # MobilePhoneModel
        f"Robot_{row_id % 10}",                          # RobotName
        f"Region_{row_id % 100}",                        # RegionName
        f"Country_{row_id % 50}",                        # CountryName
        f"City_{row_id % 200}",                          # CityName
        f"Browser_{row_id % 50}",                        # BrowserName
        f"Engine_{row_id % 20}",                         # EngineName
        f"AdvEngine_{row_id % 20}",                      # AdvEngineName
        f"Counter_{row_id % 1000}",                      # CounterName
        "example.com",                                   # Domain
        "UTF-8",                                         # PageCharset
        random.randint(1, 200),                          # TitleLength
        random.randint(1, 500),                          # URLLength
        random.randint(1, 500),                          # RefererLength
        f"Counter2_{row_id % 100}",                      # CounterName2
        f"AdvEngine2_{row_id % 20}",                     # AdvEngineName2
        f"2020-01-{1 + row_id % 30:02d}",                # EventDate2
    ]


def main():
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    print(f"Generating {NUM_ROWS:,} rows → {OUTPUT_FILE}")
    random.seed(42)  # Reproducible
    with open(OUTPUT_FILE, "w", newline="") as f:
        w = csv.writer(f, quoting=csv.QUOTE_MINIMAL)
        # Header
        w.writerow(COLUMNS)
        for i in range(NUM_ROWS):
            w.writerow(generate_row(i + 1))
            if (i + 1) % BATCH_SIZE == 0:
                print(f"  {i + 1:,} rows ({(i+1)/NUM_ROWS*100:.1f}%)")
                f.flush()
    size_gb = os.path.getsize(OUTPUT_FILE) / (1024**3)
    print(f"Done. File size: {size_gb:.2f} GB")

    # Write row count verification
    with open(DATA_DIR / "ROW_COUNT.txt", "w") as f:
        f.write(f"{NUM_ROWS}\n")


if __name__ == "__main__":
    main()
