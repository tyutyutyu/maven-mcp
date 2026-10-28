#!/usr/bin/env bash
# Opt-in smoke test against a real, pinned Apache Maven. It is not part of
# scripts/test-pyramid.sh: the prepare step needs network access once, and the
# test itself then runs Maven offline against an isolated copy of the prepared
# repository. Neither step reads ~/.m2 or the user's Maven settings.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
readonly MAVEN_VERSION="3.9.9"
readonly MAVEN_SHA512="a555254d6b53d267965a3404ecb14e53c3827c09c3b94b5678835887ab404556bfaf78dcfe03ba76fa2508649dca8531c74bca4d5846513522404d48e8c4ac8b"
readonly MAVEN_URL="https://archive.apache.org/dist/maven/maven-3/${MAVEN_VERSION}/binaries/apache-maven-${MAVEN_VERSION}-bin.tar.gz"
readonly DEPENDENCY_PLUGIN_VERSION="3.8.1"
readonly HELP_PLUGIN_VERSION="3.5.1"
readonly SUPPORTED_JAVA_MAJORS="17 21 25"
readonly CACHE_DIR="${PROJECT_ROOT}/target/real-maven-smoke"
readonly MAVEN_HOME="${CACHE_DIR}/apache-maven-${MAVEN_VERSION}"
readonly PLUGIN_REPOSITORY="${CACHE_DIR}/plugin-repository"
readonly SETTINGS="${CACHE_DIR}/settings.xml"

usage() {
  echo "Usage: scripts/real-maven-smoke.sh [--refresh]" >&2
  echo "  --refresh  delete and re-prepare the pinned Maven and plugin repository" >&2
}

refresh=false
case "${1:-}" in
  "") ;;
  --refresh) refresh=true ;;
  *) usage; exit 2 ;;
esac

cd "${PROJECT_ROOT}"

# The run must not depend on the caller's Maven configuration.
unset MAVEN_ARGS MAVEN_OPTS MAVEN_CONFIG
export MAVEN_SKIP_RC=1

for command in curl sha512sum tar cargo; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Missing real Maven smoke prerequisite: ${command}" >&2
    exit 1
  fi
done

java_command="${JAVA_HOME:+${JAVA_HOME}/bin/}java"
if ! java_version_line="$("${java_command}" -version 2>&1 | head -n 1)"; then
  echo "A JDK is required (JAVA_HOME or java on PATH); supported majors: ${SUPPORTED_JAVA_MAJORS}" >&2
  exit 1
fi
java_version="$(sed -n 's/.*version "\([^"]*\)".*/\1/p' <<<"${java_version_line}")"
java_major="${java_version%%[.+-]*}"
if [[ " ${SUPPORTED_JAVA_MAJORS} " != *" ${java_major} "* ]]; then
  echo "Unsupported JDK '${java_version_line}'; supported majors: ${SUPPORTED_JAVA_MAJORS}" >&2
  exit 1
fi

if [[ "${refresh}" == true ]]; then
  rm -rf "${CACHE_DIR}"
fi
mkdir -p "${CACHE_DIR}"

if [[ ! -x "${MAVEN_HOME}/bin/mvn" ]]; then
  archive="${CACHE_DIR}/apache-maven-${MAVEN_VERSION}-bin.tar.gz"
  curl --fail --silent --show-error --location --output "${archive}" "${MAVEN_URL}"
  echo "${MAVEN_SHA512}  ${archive}" | sha512sum --check --quiet
  tar -xzf "${archive}" -C "${CACHE_DIR}"
  rm -f "${archive}"
fi

printf '%s\n' '<settings/>' >"${SETTINGS}"

if [[ ! -d "${PLUGIN_REPOSITORY}" ]]; then
  # Resolve exactly the pinned plugins once, online, into a dedicated repository.
  seed="$(mktemp -d)"
  trap 'rm -rf "${seed}"' EXIT
  cat >"${seed}/pom.xml" <<POM
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>plugin-seed</artifactId>
  <version>1.0</version>
  <build>
    <pluginManagement>
      <plugins>
        <plugin>
          <groupId>org.apache.maven.plugins</groupId>
          <artifactId>maven-dependency-plugin</artifactId>
          <version>${DEPENDENCY_PLUGIN_VERSION}</version>
        </plugin>
        <plugin>
          <groupId>org.apache.maven.plugins</groupId>
          <artifactId>maven-help-plugin</artifactId>
          <version>${HELP_PLUGIN_VERSION}</version>
        </plugin>
      </plugins>
    </pluginManagement>
  </build>
</project>
POM
  partial="${PLUGIN_REPOSITORY}.partial"
  rm -rf "${partial}"
  (cd "${seed}" && "${MAVEN_HOME}/bin/mvn" -s "${SETTINGS}" -B -ntp -q \
    "-Dmaven.repo.local=${partial}" \
    help:effective-pom "-Doutput=${seed}/effective-pom.xml" \
    dependency:tree -DoutputType=text -Dverbose \
    dependency:build-classpath)
  mv "${partial}" "${PLUGIN_REPOSITORY}"
fi

echo "Real Maven smoke: Apache Maven ${MAVEN_VERSION}, ${java_version_line}"
MAVEN_MCP_SMOKE_MAVEN_HOME="${MAVEN_HOME}" \
  MAVEN_MCP_SMOKE_PLUGIN_REPOSITORY="${PLUGIN_REPOSITORY}" \
  MAVEN_MCP_SMOKE_SETTINGS="${SETTINGS}" \
  cargo test --locked --test real_maven_smoke -- --ignored --test-threads=1
