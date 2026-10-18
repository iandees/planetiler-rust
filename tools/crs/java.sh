#!/bin/sh
# Compile and run one of the GeoTools reference programs in this directory against the planetiler (Java) dist jar.
#
# usage: tools/crs/java.sh <ClassName> [args...]   (from the repository root; classes are compiled to target/crsgen)
#
# env: PLANETILER_JAR  planetiler-dist-*-with-deps.jar (default: a sibling planetiler checkout,
#                      ../planetiler-ref/planetiler/planetiler-dist/target/planetiler-dist-*-with-deps.jar)
#      PLANETILER_JDK  JDK home (default: javac/java on the PATH)
set -e
if [ -z "$PLANETILER_JAR" ]; then
  for f in ../planetiler-ref/planetiler/planetiler-dist/target/planetiler-dist-*-with-deps.jar; do
    [ -f "$f" ] && PLANETILER_JAR="$f"
  done
fi
if [ -z "$PLANETILER_JAR" ]; then
  echo "set PLANETILER_JAR to planetiler-dist-*-with-deps.jar" >&2
  exit 1
fi
BIN="${PLANETILER_JDK:+$PLANETILER_JDK/bin/}"
CLASS="$1"
shift
mkdir -p target/crsgen
SRC="tools/crs/$CLASS.java"
[ -f "$SRC" ] || SRC="target/crsgen/$CLASS.java"
"${BIN}javac" -nowarn -cp "$PLANETILER_JAR" -d target/crsgen "$SRC" 2>&1 | grep -v "^Note:" >&2 || true
exec "${BIN}java" -cp "$PLANETILER_JAR:target/crsgen" "$CLASS" "$@"
