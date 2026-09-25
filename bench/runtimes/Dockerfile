FROM python:3.12-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends git gcc libc6-dev curl ca-certificates unzip >/dev/null && rm -rf /var/lib/apt/lists/*
RUN pip install -q pytest==8.4.2 duckdb pytz pandas python-dateutil
# a pure-Python project with a large test suite, pinned
RUN git clone -q --depth 1 --branch v30.19.0 https://github.com/tobymao/sqlglot /opt/sqlglot
# a single large C translation unit for a compile workload
RUN (curl -fsSL -o /tmp/sqlite.zip https://www.sqlite.org/2026/sqlite-amalgamation-3530000.zip || curl -fsSL -o /tmp/sqlite.zip https://www.sqlite.org/2025/sqlite-amalgamation-3500400.zip) && cd /opt && unzip -q /tmp/sqlite.zip && mv sqlite-amalgamation-* sqlite && rm /tmp/sqlite.zip
