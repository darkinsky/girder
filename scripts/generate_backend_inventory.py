#!/usr/bin/env python3
"""Generate the stable-backend migration inventory.

The inventory deliberately measures production source instead of generated output or tests. The
scope is the backend target: stable upstream packages, protocol/SDK entry points, and host/runtime
applications. Browser UI and experimental packages are excluded explicitly so a future count cannot
silently change because a new UI package was added upstream.
"""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Iterable


SOURCE_SUFFIXES = {".c", ".cc", ".cpp", ".h", ".hpp", ".js", ".mjs", ".py", ".rs", ".ts", ".tsx"}
TEST_PARTS = {"__tests__", "test", "tests", "fixtures", "snapshots"}
TEST_SUFFIXES = (".e2e.ts", ".spec.ts", ".test.ts", ".test.tsx")

EXCLUDED_PACKAGE_DOMAINS = {"client", "experimental", "test-support"}
EXCLUDED_PACKAGE_PATHS = {"extensions/ui-cordis"}
INCLUDED_APPS = {"cli", "desktop-host", "plugin-market-generator"}
INCLUDED_TOP_LEVEL = {"native", "python"}

REMOTE_NAMESPACE = re.compile(r"""namespace\s*:\s*['"]([^'"]+)['"]""")
REMOTE_SERVICE = re.compile(r"""super\s*\(\s*[^,\n]+,\s*['"]([^'"]+)['"]""")
REMOTE_DECORATOR = re.compile(r"""^\s*@Remote(?:\s*\((.*?)\))?\s*$""")
REMOTE_NAME = re.compile(r"""name\s*:\s*['"]([^'"]+)['"]""")
METHOD = re.compile(r"""(?:async\s+)?(?:\*\s*)?([A-Za-z_$][\w$]*)\s*\(""")
GENERATED_REMOTE_FILES = {"src/api-catalog.ts"}


def is_test_file(path: Path) -> bool:
    return any(part in TEST_PARTS for part in path.parts) or path.name.endswith(TEST_SUFFIXES)


def source_files(root: Path, *, source_only: bool = False) -> Iterable[Path]:
    base = root / "src" if source_only and (root / "src").is_dir() else root
    for path in sorted(base.rglob("*")):
        if (
            path.is_file()
            and path.suffix in SOURCE_SUFFIXES
            and "node_modules" not in path.parts
            and "lib" not in path.parts
            and "dist" not in path.parts
            and "build" not in path.parts
            and not is_test_file(path)
        ):
            yield path


def count_lines(paths: Iterable[Path]) -> tuple[int, int]:
    files = 0
    lines = 0
    for path in paths:
        files += 1
        lines += sum(1 for _ in path.open(encoding="utf-8", errors="ignore"))
    return files, lines


def package_records(upstream: Path) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    packages = upstream / "packages"
    for domain in sorted(packages.iterdir()):
        if not domain.is_dir() or domain.name in EXCLUDED_PACKAGE_DOMAINS:
            continue
        for package in sorted(domain.iterdir()):
            if not package.is_dir() or not (package / "package.json").is_file():
                continue
            relative = f"{domain.name}/{package.name}"
            if relative in EXCLUDED_PACKAGE_PATHS:
                continue
            metadata = json.loads((package / "package.json").read_text(encoding="utf-8"))
            paths = list(source_files(package, source_only=True))
            files, lines = count_lines(paths)
            records.append(
                {
                    "path": f"packages/{relative}",
                    "name": metadata.get("name"),
                    "files": files,
                    "lines": lines,
                }
            )
    return records


def application_records(upstream: Path) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for top_level, names in (("apps", INCLUDED_APPS),):
        for name in sorted(names):
            root = upstream / top_level / name
            files, lines = count_lines(source_files(root))
            records.append({"path": f"{top_level}/{name}", "files": files, "lines": lines})

    for top_level in sorted(INCLUDED_TOP_LEVEL):
        root = upstream / top_level
        files, lines = count_lines(source_files(root))
        records.append({"path": top_level, "files": files, "lines": lines})
    return records


def remote_records(upstream: Path) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for package in package_records(upstream):
        root = upstream / package["path"]
        for path in source_files(root, source_only=True):
            if path.suffix not in {".ts", ".tsx"}:
                continue
            if package["path"] == "packages/extensions/tool-cordis" and path.name in GENERATED_REMOTE_FILES:
                continue
            text = path.read_text(encoding="utf-8", errors="ignore")
            lines = text.splitlines()
            decorated = [(index, match) for index, line in enumerate(lines)
                         if (match := REMOTE_DECORATOR.match(line)) is not None]
            if not decorated:
                continue
            # TypertRemoteService defaults its gateway namespace to the service key. Some
            # services override it in a third `super()` options object. The source is formatted
            # across multiple lines, so inspect a bounded class/constructor region instead of
            # requiring the complete call on one line.
            namespace_matches = REMOTE_NAMESPACE.findall(text)
            service_matches = REMOTE_SERVICE.findall(text)
            namespaces = sorted(set(namespace_matches or service_matches))
            methods: list[str] = []
            for index, decorator in decorated:
                options = decorator.group(1) or ""
                alias = None
                quoted = re.fullmatch(r"""\s*['"]([^'"]+)['"]\s*""", options)
                if quoted:
                    alias = quoted.group(1)
                else:
                    named = REMOTE_NAME.search(options)
                    if named:
                        alias = named.group(1)
                for candidate in lines[index + 1 : index + 8]:
                    match = METHOD.search(candidate)
                    if match:
                        methods.append(alias or match.group(1))
                        break
            records.append(
                {
                    "package": package["path"],
                    "file": str(path.relative_to(root)),
                    "namespaces": namespaces,
                    "methods": sorted(set(methods)),
                }
            )
    return records


def rust_records(girder: Path) -> dict[str, int]:
    source = list((girder / "crates").rglob("*.rs"))
    production = [path for path in source if "tests" not in path.parts]
    tests = [path for path in source if "tests" in path.parts]
    source_files_count, source_lines = count_lines(production)
    test_files_count, test_lines = count_lines(tests)
    return {
        "production_files": source_files_count,
        "production_lines": source_lines,
        "test_files": test_files_count,
        "test_lines": test_lines,
    }


def rust_declared_methods(girder: Path) -> set[str]:
    surface = girder / "crates/girder-core/src/api/method_surface.rs"
    text = surface.read_text(encoding="utf-8", errors="ignore")
    return set(re.findall(r'method:\s*"([^"]+)"', text))


def build_inventory(upstream: Path, girder: Path) -> dict[str, object]:
    packages = package_records(upstream)
    applications = application_records(upstream)
    remotes = remote_records(upstream)
    upstream_methods = {
        f"{namespace}.{method}"
        for item in remotes
        for namespace in item["namespaces"]
        for method in item["methods"]
    }
    girder_methods = rust_declared_methods(girder)
    return {
        "scope": {
            "included": [
                "stable packages/*/* except client, experimental, test-support, extensions/ui-cordis",
                "apps/cli",
                "apps/desktop-host",
                "apps/plugin-market-generator",
                "native",
                "python",
            ],
            "excluded": [
                "packages/client",
                "packages/experimental",
                "packages/test-support",
                "packages/extensions/ui-cordis",
                "apps/web",
                "apps/desktop",
            ],
        },
        "upstream": {
            "packages": packages,
            "applications": applications,
            "remote_sources": remotes,
            "totals": {
                "package_files": sum(int(item["files"]) for item in packages),
                "package_lines": sum(int(item["lines"]) for item in packages),
                "application_files": sum(int(item["files"]) for item in applications),
                "application_lines": sum(int(item["lines"]) for item in applications),
                "remote_files": len(remotes),
                "remote_methods": sum(len(item["methods"]) for item in remotes),
                "remote_namespaces": sorted(
                    {namespace for item in remotes for namespace in item["namespaces"]}
                ),
            },
        },
        "girder": rust_records(girder),
        "surface_comparison": {
            "upstream_remote_methods": sorted(upstream_methods),
            "girder_declared_methods": sorted(girder_methods),
            "matched": sorted(upstream_methods & girder_methods),
            "missing_in_girder": sorted(upstream_methods - girder_methods),
            "extra_in_girder": sorted(girder_methods - upstream_methods),
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--upstream",
        type=Path,
        default=Path("/data/workspace/github/deepseek-harness"),
        help="path to the reference DeepSeek Harness checkout",
    )
    parser.add_argument(
        "--girder",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="path to the Girder checkout",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="write JSON to this path instead of stdout",
    )
    args = parser.parse_args()
    inventory = build_inventory(args.upstream.resolve(), args.girder.resolve())
    rendered = json.dumps(inventory, indent=2, ensure_ascii=False) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")


if __name__ == "__main__":
    main()
