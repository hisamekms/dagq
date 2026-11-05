#!/bin/sh
# Builds PostgreSQL 18.6 with pg_trgm and pg_bigm under WORK and starts it on
# 127.0.0.1:55433 with the locale en_US.UTF-8, without a Unix socket. It installs
# nothing outside WORK; the database listens until `pg_ctl -D WORK/data stop`.
#
#   sh build-pg.sh WORK
set -eu
work=$1
mkdir -p "$work/src"
cd "$work/src"
curl -sSfO https://ftp.postgresql.org/pub/source/v18.6/postgresql-18.6.tar.bz2
curl -sSfL -o pg_bigm.tar.gz https://github.com/pgbigm/pg_bigm/archive/refs/tags/v1.2-20250903.tar.gz
tar xjf postgresql-18.6.tar.bz2
tar xzf pg_bigm.tar.gz
(
  cd postgresql-18.6
  ./configure --prefix="$work/pg" --without-icu --without-readline --without-zlib --without-zstd --without-lz4
  make -j3 -s
  make -s install
  make -s -C contrib/pg_trgm install
)
make -C pg_bigm-1.2-20250903 USE_PGXS=1 PG_CONFIG="$work/pg/bin/pg_config" -s install
LC_ALL=en_US.UTF-8 "$work/pg/bin/initdb" -D "$work/data" -U spike --auth=trust --encoding=UTF8 --locale=en_US.UTF-8
"$work/pg/bin/pg_ctl" -D "$work/data" -w -l "$work/pg.log" \
  -o "-p 55433 -c listen_addresses=127.0.0.1 -c unix_socket_directories='' -c shared_buffers=128MB" start
