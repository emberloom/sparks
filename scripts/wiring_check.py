#!/usr/bin/env python3
"""
Wiring & plumbing check for Sparks.

Validates that all declared enums/variants/implementations are actually wired
into the runtime — catching loose ends before they become silent dead weight.

Checks:
  1. Every ObserverCategory variant is emitted at least once.
  2. Every non-test PulseSource variant is used in at least one Pulse::new() call.
  3. Every LlmProvider implementation is referenced in build_llm_provider_for().
  4. Every ticket intake provider module has a registration in core.rs dispatch.
  5. Every PulseSource variant has a label() match arm.
  6. Every ObserverCategory variant has a label() and color() match arm.

Exit code 0 = all checks pass.
Exit code 1 = one or more wiring violations found.
"""

import re
import sys
from pathlib import Path

SRC = Path(__file__).parent.parent / "src"

RESET = "\033[0m"
RED = "\033[1;31m"
GREEN = "\033[1;32m"
YELLOW = "\033[1;33m"
CYAN = "\033[1;36m"
DIM = "\033[2m"

failures: list[str] = []
passes: list[str] = []


def read(rel: str) -> str:
    return (SRC / rel).read_text()


def read_all_rs() -> str:
    """Concatenate all .rs source files for cross-file grep."""
    parts = []
    for p in sorted(SRC.rglob("*.rs")):
        parts.append(p.read_text())
    return "\n".join(parts)


def fail(check: str, detail: str) -> None:
    failures.append(f"{check}: {detail}")


def ok(check: str) -> None:
    passes.append(check)


# ---------------------------------------------------------------------------
# 1. ObserverCategory — every variant must be emitted
# ---------------------------------------------------------------------------
def check_observer_categories() -> None:
    observer_src = read("observer.rs")
    # Extract variant names from the enum block
    enum_match = re.search(
        r"pub enum ObserverCategory \{([^}]+)\}", observer_src, re.DOTALL
    )
    if not enum_match:
        fail("ObserverCategory", "Could not locate enum definition in observer.rs")
        return

    variants = re.findall(r"^\s+(\w+),?$", enum_match.group(1), re.MULTILINE)
    if not variants:
        fail("ObserverCategory", "No variants found in enum")
        return

    all_src = read_all_rs()
    missing = []
    for v in variants:
        pattern = f"ObserverCategory::{v}"
        # The enum body uses bare variant names (e.g. `Startup,`), not `ObserverCategory::Startup`.
        # So any match of `ObserverCategory::X` is an actual emit site.
        uses = re.findall(re.escape(pattern), all_src)
        if len(uses) == 0:
            missing.append(v)

    if missing:
        for v in missing:
            fail("ObserverCategory::emit", f"{v} declared but never emitted")
    else:
        ok(f"ObserverCategory — all {len(variants)} variants emitted")

    # Also check label() and color() have arms for each variant
    for method in ("label", "color"):
        missing_arm = [v for v in variants if f"Self::{v} =>" not in observer_src]
        if missing_arm:
            for v in missing_arm:
                fail(f"ObserverCategory::{method}()", f"Missing match arm for {v}")
        else:
            ok(f"ObserverCategory::{method}() — all arms present")


# ---------------------------------------------------------------------------
# 2. PulseSource — every non-cfg(test) variant must appear in Pulse::new()
# ---------------------------------------------------------------------------
def check_pulse_sources() -> None:
    pulse_src = read("pulse.rs")

    enum_match = re.search(
        r"pub enum PulseSource \{([^}]+)\}", pulse_src, re.DOTALL
    )
    if not enum_match:
        fail("PulseSource", "Could not locate enum definition in pulse.rs")
        return

    enum_body = enum_match.group(1)
    # Parse variants, tracking whether they have #[cfg(test)] above them
    runtime_variants = []
    lines = enum_body.splitlines()
    cfg_test_next = False
    for line in lines:
        stripped = line.strip()
        if stripped == "#[cfg(test)]":
            cfg_test_next = True
            continue
        m = re.match(r"(\w+)(?:\([^)]*\))?,?$", stripped)
        if m:
            vname = m.group(1)
            if not cfg_test_next:
                runtime_variants.append(vname)
            cfg_test_next = False
        else:
            cfg_test_next = False

    if not runtime_variants:
        fail("PulseSource", "No runtime variants found")
        return

    all_src = read_all_rs()
    missing = []
    for v in runtime_variants:
        pattern = f"PulseSource::{v}"
        # Must appear somewhere outside its own declaration
        uses = re.findall(re.escape(pattern), all_src)
        # Declaration line itself + label() arm = 2 hits minimum before any Pulse::new
        # Check for actual Pulse::new usage — match both short and fully-qualified paths
        # e.g. both `PulseSource::Heartbeat` and `crate::pulse::PulseSource::Heartbeat`
        pulse_new_pattern = (
            rf"Pulse::new\s*\(\s*(?:crate::pulse::)?PulseSource::{re.escape(v)}"
        )
        if not re.search(pulse_new_pattern, all_src):
            missing.append(v)

    if missing:
        for v in missing:
            fail("PulseSource::Pulse::new", f"{v} never used in Pulse::new()")
    else:
        ok(f"PulseSource — all {len(runtime_variants)} runtime variants constructed")

    # Check label() arms
    missing_label = [
        v for v in runtime_variants if f"Self::{v}" not in pulse_src
    ]
    if missing_label:
        for v in missing_label:
            fail("PulseSource::label()", f"Missing match arm for {v}")
    else:
        ok("PulseSource::label() — all arms present")


# ---------------------------------------------------------------------------
# 3. LlmProvider — every impl must appear in build_llm_provider_for()
# ---------------------------------------------------------------------------
def check_llm_providers() -> None:
    llm_src = read("llm.rs")
    config_src = read("config.rs")

    impl_types = re.findall(r"impl LlmProvider for (\w+)", llm_src)
    if not impl_types:
        fail("LlmProvider", "No implementations found in llm.rs")
        return

    missing = []
    for t in impl_types:
        if t not in config_src:
            missing.append(t)

    if missing:
        for t in missing:
            fail(
                "LlmProvider::build_llm_provider_for",
                f"{t} implements LlmProvider but is not referenced in config.rs",
            )
    else:
        ok(f"LlmProvider — all {len(impl_types)} implementations registered in config.rs")


# ---------------------------------------------------------------------------
# 4. Ticket intake providers — every provider module must be dispatched
# ---------------------------------------------------------------------------
def check_ticket_intake_providers() -> None:
    ti_dir = SRC / "ticket_intake"
    provider_modules = [
        p.stem
        for p in ti_dir.glob("*.rs")
        if p.stem not in ("mod", "provider", "store", "sync", "webhook")
    ]

    if not provider_modules:
        fail("TicketIntake", "No provider modules found in src/ticket_intake/")
        return

    core_src = read("core.rs")
    missing = []
    for mod in provider_modules:
        # Look for the module name used as a provider identifier in core.rs
        # e.g. ticket_intake::github or "github" string literal
        if f"ticket_intake::{mod}" not in core_src and f'"{mod}"' not in core_src:
            missing.append(mod)

    if missing:
        for mod in missing:
            fail(
                "TicketIntake::dispatch",
                f"Provider module '{mod}' not referenced in core.rs",
            )
    else:
        ok(
            f"TicketIntake — all {len(provider_modules)} provider modules "
            f"referenced in core.rs: {', '.join(sorted(provider_modules))}"
        )


# ---------------------------------------------------------------------------
# 5. Smoke-check: PulseBus is subscribed at least once (delivery path exists)
# ---------------------------------------------------------------------------
def check_pulse_delivery_path() -> None:
    all_src = read_all_rs()
    if "bus.subscribe()" not in all_src and "PulseBus" not in all_src:
        fail("PulseBus", "PulseBus never subscribed — pulses have no consumer")
    else:
        ok("PulseBus — subscription path present")


# ---------------------------------------------------------------------------
# 6. Executor has ObserverHandle (ToolUsage wired)
# ---------------------------------------------------------------------------
def check_executor_observer() -> None:
    executor_src = read("executor.rs")
    if "ObserverHandle" not in executor_src:
        fail(
            "Executor::observer",
            "Executor does not hold an ObserverHandle — ToolUsage cannot be emitted",
        )
    elif "ObserverCategory::ToolUsage" not in executor_src:
        fail(
            "Executor::ToolUsage",
            "Executor has ObserverHandle but never emits ObserverCategory::ToolUsage",
        )
    else:
        ok("Executor — ObserverHandle present and ToolUsage emitted")


# ---------------------------------------------------------------------------
# 7. MoodState::drift emits EnergyShift
# ---------------------------------------------------------------------------
def check_energy_shift_wired() -> None:
    mood_src = read("mood.rs")
    if "ObserverCategory::EnergyShift" not in mood_src:
        fail(
            "MoodState::drift::EnergyShift",
            "mood.rs::drift() does not emit ObserverCategory::EnergyShift",
        )
    else:
        ok("MoodState::drift — EnergyShift emitted")


# ---------------------------------------------------------------------------
# Run all checks
# ---------------------------------------------------------------------------
def main() -> int:
    print(f"{CYAN}Sparks wiring check{RESET}")
    print(f"{DIM}src: {SRC}{RESET}\n")

    check_observer_categories()
    check_pulse_sources()
    check_llm_providers()
    check_ticket_intake_providers()
    check_pulse_delivery_path()
    check_executor_observer()
    check_energy_shift_wired()

    for msg in passes:
        print(f"  {GREEN}✓{RESET} {msg}")

    if failures:
        print()
        for msg in failures:
            print(f"  {RED}✗{RESET} {msg}")
        print(
            f"\n{RED}Wiring check FAILED — {len(failures)} violation(s){RESET}"
        )
        return 1

    print(f"\n{GREEN}All wiring checks passed ({len(passes)} checks){RESET}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
