"""Monotonic release-alias decisions for the GHCR publication workflow."""

from __future__ import annotations

import argparse
import re


VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def version(value: str) -> tuple[int, int, int]:
    if not VERSION_RE.fullmatch(value):
        raise ValueError(f"invalid release version: {value!r}")
    return tuple(int(part) for part in value.split("."))  # type: ignore[return-value]


def alias_action(alias: str, candidate: str, existing: str | None) -> str:
    candidate_version = version(candidate)
    if not existing:
        return "advance"
    existing_version = version(existing)
    if alias == "minor":
        if existing_version[:2] != candidate_version[:2]:
            raise ValueError(
                f"minor alias {candidate_version[0]}.{candidate_version[1]} "
                f"points at unrelated version {existing}"
            )
        return "advance" if candidate_version >= existing_version else "keep"
    if alias == "latest":
        return "advance" if candidate_version >= existing_version else "keep"
    raise ValueError(f"unknown alias kind: {alias!r}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--alias", required=True, choices=("minor", "latest"))
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--existing")
    args = parser.parse_args()
    print(alias_action(args.alias, args.candidate, args.existing))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
