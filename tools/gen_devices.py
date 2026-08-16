#!/usr/bin/env python3
"""Generate the redistributable device registry from local analysis extracts."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


PID_RE = re.compile(r"046d_([0-9a-fA-F]{4})\Z")
GKEY_RE = re.compile(r"(?:^|_)g(\d+)_m\d+(?:_|$)")
GKEY_LAYOUT_COUNTS = {
    "GKEY_LEFT_COLUMN_5": 5,
    "GKEY_LEFT_COLUMN_5_TOP_ROW_4": 9,
    "GKEY_LEFT_COLUMN_6": 6,
    "GKEY_FUNCTION_KEYS_ROW_12": 12,
}


def load_json(path: Path) -> Any:
    with path.open(encoding="utf-8") as source:
        return json.load(source)


def depot_names(device: dict[str, Any], model_index: dict[str, list[str]]) -> list[str]:
    names = list(model_index.get(device["modelId"], []))
    depot = device.get("depot")
    if depot:
        names.append(depot)
    names.extend((device.get("depots") or {}).values())
    return list(dict.fromkeys(names))


def metadata_for(names: list[str], root: Path, cache: dict[str, dict[str, Any]]) -> list[dict[str, Any]]:
    result = []
    for name in names:
        if name not in cache:
            path = root / name / "metadata.json"
            cache[name] = load_json(path) if path.is_file() else {}
        if cache[name]:
            result.append(cache[name])
    return result


def metadata_slot_ids(metadata: list[dict[str, Any]]) -> set[str]:
    return {
        assignment["slotId"]
        for document in metadata
        for image in document.get("images", [])
        for assignment in image.get("assignments", [])
        if isinstance(assignment.get("slotId"), str)
    }


def gkey_count(
    device: dict[str, Any],
    depots: list[dict[str, Any]],
    metadata: list[dict[str, Any]],
) -> int | None:
    capabilities = device.get("capabilities") or {}
    input_support = capabilities.get("inputSupport") or device.get("inputSupport") or {}
    if "G_KEY" not in input_support.get("supportedCategories", []):
        return None

    slot_ids = {slot for depot in depots for slot in depot.get("slotIds", [])}
    slot_ids.update(metadata_slot_ids(metadata))
    ordinals = {
        int(match.group(1))
        for slot in slot_ids
        if (match := GKEY_RE.search(slot))
    }
    if ordinals:
        return len(ordinals)

    layout_count = GKEY_LAYOUT_COUNTS.get(capabilities.get("gkeyLayout"))
    if layout_count is not None:
        return layout_count

    summaries = [
        depot["default_configurations"]
        for depot in depots
        if depot.get("default_configurations") is not None
    ]
    for summary in summaries:
        total = summary.get("g_key_count")
        layers = summary.get("layers") or []
        if total == 0:
            return 0
        if isinstance(total, int) and layers and total % len(layers) == 0:
            return total // len(layers)
    return None


def hid_usage_label(usage: int) -> str:
    if 0x04 <= usage <= 0x1D:
        return chr(ord("A") + usage - 0x04)
    if 0x1E <= usage <= 0x26:
        return str(usage - 0x1D)
    if usage == 0x27:
        return "0"
    if 0x3A <= usage <= 0x45:
        return f"F{usage - 0x39}"
    if 0x68 <= usage <= 0x73:
        return f"F{usage - 0x5B}"
    if 0x59 <= usage <= 0x61:
        return f"Keypad {usage - 0x58}"
    if 0xE0 <= usage <= 0xE7:
        return (
            "Left Ctrl",
            "Left Shift",
            "Left Alt",
            "Left GUI",
            "Right Ctrl",
            "Right Shift",
            "Right Alt",
            "Right GUI",
        )[usage - 0xE0]
    labels = {
        0x28: "Enter",
        0x29: "Escape",
        0x2A: "Backspace",
        0x2B: "Tab",
        0x2C: "Space",
        0x2D: "Minus",
        0x2E: "Equal",
        0x2F: "Left Bracket",
        0x30: "Right Bracket",
        0x31: "Backslash",
        0x32: "Non-US Hash",
        0x33: "Semicolon",
        0x34: "Quote",
        0x35: "Grave",
        0x36: "Comma",
        0x37: "Period",
        0x38: "Slash",
        0x39: "Caps Lock",
        0x46: "Print Screen",
        0x47: "Scroll Lock",
        0x48: "Pause",
        0x49: "Insert",
        0x4A: "Home",
        0x4B: "Page Up",
        0x4C: "Delete",
        0x4D: "End",
        0x4E: "Page Down",
        0x4F: "Right Arrow",
        0x50: "Left Arrow",
        0x51: "Down Arrow",
        0x52: "Up Arrow",
        0x53: "Num Lock",
        0x54: "Keypad Slash",
        0x55: "Keypad Asterisk",
        0x56: "Keypad Minus",
        0x57: "Keypad Plus",
        0x58: "Keypad Enter",
        0x62: "Keypad 0",
        0x63: "Keypad Period",
        0x64: "Non-US Backslash",
        0x65: "Application",
        0x66: "Power",
        0x67: "Keypad Equal",
        0x74: "Execute",
        0x75: "Help",
        0x76: "Menu",
        0x77: "Select",
        0x78: "Stop",
        0x79: "Again",
        0x7A: "Undo",
        0x7B: "Cut",
        0x7C: "Copy",
        0x7D: "Paste",
        0x7E: "Find",
        0x7F: "Mute",
        0x80: "Volume Up",
        0x81: "Volume Down",
        0x82: "Locking Caps Lock",
        0x83: "Locking Num Lock",
        0x84: "Locking Scroll Lock",
        0x85: "Keypad Comma",
        0x86: "Keypad Equal AS/400",
        0x87: "International 1",
        0x88: "International 2",
        0x89: "International 3",
        0x8A: "International 4",
        0x8B: "International 5",
    }
    return labels.get(usage, f"HID usage 0x{usage:02X}")


def per_key_map(metadata: list[dict[str, Any]]) -> dict[str, dict[str, str]] | None:
    usages = {
        component["id"]
        for document in metadata
        for image in document.get("images", [])
        for zone in image.get("zones", [])
        if zone.get("id") == "PERKEY_KEYBOARD"
        for component in zone.get("components", [])
        if isinstance(component.get("id"), int)
    }
    if not usages:
        return None
    # 0x8081 wire zone ids: verified on a G915/G913 (2026-08-16) to be the Solaar-style
    # sequential table (A=1, zone = HID usage - 3), NOT the raw HID usage. All known per-key
    # boards use the same 0x8081 v2 feature, so default every per-key map to "solaar";
    # `--zone-scheme hidusage` remains available as an override.
    return {
        "zone_scheme": "solaar",
        **{
        str(usage): {
            "label": hid_usage_label(usage),
            "component": f"PERKEY_KEYBOARD_{usage:02x}",
        }
        for usage in sorted(usages)
        },
    }


def dpi_default(depots: list[dict[str, Any]]) -> dict[str, Any] | None:
    for depot in depots:
        override = depot.get("defaults_override") or {}
        tables = override.get("dpi_tables") or []
        if tables:
            table = tables[0]
            return {
                "levels": table["levels"],
                "default": table["defaultDpi"],
                "shift": table["shiftDpi"],
            }
    return None


def generate(
    registry: dict[str, Any],
    depot_table: dict[str, dict[str, Any]],
    slot_ids: dict[str, Any],
    model_index: dict[str, list[str]],
    depots_root: Path,
) -> dict[str, Any]:
    metadata_cache: dict[str, dict[str, Any]] = {}
    devices = []
    for source in registry["devices"]:
        capabilities = source.get("capabilities") or {}
        lighting = capabilities.get("lightingSupport") or {}
        input_support = capabilities.get("inputSupport") or source.get("inputSupport") or {}
        names = depot_names(source, model_index)
        depots = [depot_table[name] for name in names if name in depot_table]
        metadata = metadata_for(names, depots_root, metadata_cache)
        category = lighting.get("deviceCategory")
        is_per_key = bool(
            lighting.get("isPerKey") or (category and category.endswith("_PER_KEY"))
        )
        pids = []
        for mode in source.get("modes", []):
            for interface in mode.get("interfaces", []):
                match = PID_RE.fullmatch(interface.get("id", ""))
                if match:
                    pid = f"0x{match.group(1).lower()}"
                    if pid not in pids:
                        pids.append(pid)
        devices.append(
            {
                "model_id": source["modelId"],
                "display_name": source["displayName"],
                "type": source["type"],
                "pids": pids,
                "slot_prefix": source.get("slotPrefix")
                or slot_ids.get("slot_prefix_by_model_id", {}).get(source["modelId"]),
                "lighting": {
                    "category": category,
                    "per_key": is_per_key,
                    "zones": [
                        {
                            "zone_type": zone["zoneType"],
                            "effects": [effect["id"] for effect in zone.get("supportedEffects", [])],
                        }
                        for zone in lighting.get("zones", [])
                    ],
                    "persistence": lighting.get("persistence") or {},
                },
                "input": {
                    "categories": input_support.get("supportedCategories", []),
                    "layers": input_support.get("supportedLayers", []),
                },
                "gkeys": {"count": gkey_count(source, depots, metadata)},
                "onboard": {
                    "supported": bool((capabilities.get("onboardProfiles") or {}).get("supportsOnboardMode"))
                },
                "dpi_default": dpi_default(depots),
                "per_key_map": per_key_map(metadata)
                if source["type"] == "KEYBOARD" and is_per_key
                else None,
            }
        )
    return {"devices": devices}


def main() -> None:
    repo_root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--analysis-root", type=Path, default=Path.home() / "ghub_re" / "analysis")
    parser.add_argument("--depots-root", type=Path, default=Path.home() / "ghub_re" / "depots_all")
    parser.add_argument("--output", type=Path, default=repo_root / "data" / "devices.json")
    args = parser.parse_args()

    tables = args.analysis_root / "tables"
    registry = load_json(tables / "device_registry.json")
    depot_table = load_json(tables / "device_depots.json")
    slot_ids = load_json(tables / "slot_ids.json")
    model_index = load_json(tables / "model_index.json")
    result = generate(registry, depot_table, slot_ids, model_index, args.depots_root)

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8", newline="\n") as destination:
        json.dump(result, destination, ensure_ascii=False, indent=2)
        destination.write("\n")

    devices = result["devices"]
    mapped = sum(device["per_key_map"] is not None for device in devices)
    dpi = sum(device["dpi_default"] is not None for device in devices)
    print(f"Generated {len(devices)} devices ({mapped} with per_key_map, {dpi} with dpi_default)")


if __name__ == "__main__":
    main()
