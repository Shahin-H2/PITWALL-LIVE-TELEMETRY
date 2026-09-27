#!/usr/bin/env python3
"""
AI Race Engineer — the narrative layer over the physics layer.

    python3 ai/tune_engineer.py captures/session.json
    python3 ai/tune_engineer.py captures/session.json --offline
    python3 ai/tune_engineer.py captures/session.json --track "Spa" --car "GT3"

Architecture, and why it is split this way
------------------------------------------
A language model asked to produce a car setup from telemetry will produce
confident, fluent, plausible numbers — and nothing in the output distinguishes
the right ones from the wrong ones. That is unacceptable when the output is a
change a driver makes to a car before a session.

So the work is divided:

    Rust `telemetry-analysis`  ->  WHAT changes, in WHICH direction, and WHY
    this script (Claude)       ->  in WHAT ORDER, and HOW to explain it

The model receives the physics layer's conclusions as established fact. It is
explicitly instructed not to invent parameters, and `validate()` below rejects
the response if it does. Every recommendation the driver sees traces back to a
measurement.

If there is no API key, or the call fails, `--offline` output is still a
complete and correct engineering report. The model makes it better; it is not
load-bearing.
"""

import argparse
import json
import os
import sys
import textwrap

MODEL = "claude-opus-5"

# --------------------------------------------------------------------------
# The physics knowledge the model reasons with.
#
# Kept as a frozen block at the head of the system prompt so it caches cleanly:
# prompt caching is a prefix match, so anything volatile (the session data)
# must come after this, never inside it.
# --------------------------------------------------------------------------
VEHICLE_DYNAMICS = """\
You are a race engineer. You are given telemetry that has already been analysed
by a deterministic physics layer. Your job is to prioritise and explain, not to
recompute.

REFERENCE — the causal relationships behind the analysis:

Anti-roll bars
  Stiffening a bar increases lateral load transfer across that axle and REDUCES
  that axle's grip. Stiffer front -> more understeer. Stiffer rear -> more
  oversteer. Softening does the reverse. This is the fastest lever for balance
  and it costs nothing in drag.

Aerodynamics
  More wing on an axle = more downforce on that axle at speed, and more drag.
  Front wing sharpens turn-in but is speed-dependent: it does little at 60 km/h
  and a great deal at 250. Rear wing adds entry and mid-corner stability and
  costs top speed. Aero changes affect fast corners far more than slow ones.

Springs and ride height
  Softer springs on an axle generally increase mechanical grip on that axle but
  slow the transient response and risk bottoming. Ride height and rake alter
  aero platform; lowering the front generally adds front downforce.

Differential
  Higher POWER lock improves traction on corner exit but resists rotation and
  induces mid-exit understeer. Higher COAST lock stabilises the car under
  deceleration but can create entry understeer. Lower coast lock frees the rear
  on entry, which suits trail-brakers.

Brake bias
  Rearward bias helps the car rotate under braking, at the cost of stability
  and rear lock-up risk. It matters far more to a driver who trail-brakes than
  to one who finishes braking in a straight line.

Camber
  More negative camber increases the contact patch at full lateral load,
  reducing mid-corner push, at the cost of straight-line braking and tyre wear.

Gearing
  Time spent on the limiter before a braking zone is wasted acceleration:
  lengthen the final drive. Never reaching the top of the power band in the
  highest used gear means the ratio is too long: shorten it.

DRIVING STYLE MATTERS AND CHANGES THE ANSWER
  A trail-braker rotates the car with the brake pedal and is slowed down by a
  setup that adds entry stability. A straight-line braker turns in on a neutral
  throttle and needs that same stability to be fast. The same balance
  measurement can warrant opposite advice for these two drivers. Read the
  driver profile before you prioritise.

COAST TIME IS A DRIVING PROBLEM, NOT A SETUP PROBLEM
  Time with neither pedal applied is dead time. No differential setting
  recovers it. Say so plainly rather than proposing a car change.

RULES YOU MUST FOLLOW
  1. Only reference parameters present in `physics_recommendations`. Do not
     introduce a parameter the physics layer did not identify.
  2. Do not invent numeric setup values (clicks, wing levels, PSI, ratios).
     The sim's valid ranges are not known to you and a wrong number is worse
     than no number. Speak in directions and relative magnitudes.
  3. Respect the stated confidence. A `low` confidence item is a hypothesis to
     test, and must be described as one.
  4. If the data does not support a change, say the car is fine. Inventing work
     costs the driver track time.
  5. Order changes by expected lap-time gain. One change at a time, so the
     driver can feel what each one did.
"""

RESPONSE_SCHEMA = {
    "type": "object",
    "properties": {
        "verdict": {
            "type": "string",
            "description": "One sentence: what is actually limiting this driver.",
        },
        "priority_changes": {
            "type": "array",
            "description": "Setup changes, most valuable first. Only parameters from physics_recommendations.",
            "items": {
                "type": "object",
                "properties": {
                    "parameter": {"type": "string"},
                    "direction": {"type": "string", "enum": ["Increase", "Decrease"]},
                    "magnitude": {"type": "string", "enum": ["small", "medium", "large"]},
                    "confidence": {"type": "string", "enum": ["low", "medium", "high"]},
                    "why": {"type": "string", "description": "Plain-language reason, citing the measurement."},
                    "what_to_feel": {"type": "string", "description": "How the driver will know it worked."},
                },
                "required": ["parameter", "direction", "magnitude", "confidence", "why", "what_to_feel"],
                "additionalProperties": False,
            },
        },
        "driving_advice": {
            "type": "array",
            "description": "Technique changes. Empty if none apply.",
            "items": {
                "type": "object",
                "properties": {
                    "focus": {"type": "string"},
                    "detail": {"type": "string"},
                    "estimated_gain_s": {"type": ["number", "null"]},
                },
                "required": ["focus", "detail", "estimated_gain_s"],
                "additionalProperties": False,
            },
        },
        "test_plan": {
            "type": "string",
            "description": "How to validate the changes over the next runs.",
        },
    },
    "required": ["verdict", "priority_changes", "driving_advice", "test_plan"],
    "additionalProperties": False,
}


def build_user_message(data, track, car):
    """The volatile half of the prompt — everything that changes per session.

    Deliberately placed after the cached system prompt: prompt caching matches
    on a prefix, so putting session data before the frozen block would
    invalidate the cache on every single run.
    """
    ctx = []
    if track:
        ctx.append(f"Track: {track}")
    if car:
        ctx.append(f"Car: {car}")
    ctx.append(f"Simulator: {data.get('simulator', 'unknown')}")

    return (
        "\n".join(ctx)
        + "\n\nHere is the analysed session. The `physics_recommendations` were "
        "derived deterministically from the telemetry and are established fact — "
        "prioritise and explain them; do not add to them.\n\n"
        + "```json\n"
        + json.dumps(data, indent=2)
        + "\n```\n\n"
        "Give me the engineering call."
    )


def validate(result, data):
    """Reject a response that stepped outside the physics layer's conclusions.

    This is the guardrail that makes the whole arrangement trustworthy. Without
    it, "the model only explains, it never invents" is an intention rather than
    a property.
    """
    allowed = {r["parameter"] for r in data.get("physics_recommendations", [])}
    physics_by_name = {r["parameter"]: r for r in data.get("physics_recommendations", [])}
    problems = []

    for c in result.get("priority_changes", []):
        name = c.get("parameter", "")
        if name not in allowed:
            problems.append(f"invented parameter {name!r} (not in the physics layer's output)")
            continue
        expected = physics_by_name[name]["direction"]
        if c.get("direction") != expected:
            problems.append(
                f"{name}: model said {c.get('direction')}, physics said {expected}"
            )
    return problems


def offline_report(data):
    """The physics layer's own output, formatted. No model involved."""
    print("=" * 72)
    print("ENGINEERING REPORT  (offline — physics layer only)")
    print("=" * 72)
    d = data.get("driver") or {}
    if d:
        print(f"\nDriver profile : {d.get('profile', 'n/a')}")
        print(f"Corners        : {d.get('corners_analysed', 0)}")
        print(f"Coast per lap  : {d.get('total_coast_s', 0):.2f}s")
        for k, label in (
            ("balance_entry_deg", "Balance entry"),
            ("balance_apex_deg", "Balance apex"),
            ("balance_exit_deg", "Balance exit"),
        ):
            v = d.get(k)
            if v is not None:
                trait = "oversteer" if v > 1.15 else "understeer" if v < -1.15 else "neutral"
                print(f"{label:<15}: {v:+.2f}°  ({trait})")

    print(f"\nGearing        : {data.get('gearing', {}).get('headline', 'n/a')}")

    recs = data.get("physics_recommendations", [])
    print(f"\nSetup changes  : {len(recs)}")
    for r in recs:
        arrow = "^" if r["direction"] == "Increase" else "v"
        print(f"  {arrow} {r['parameter']:<24} {r['magnitude']:<7} conf={r['confidence']}")
        print(f"      {r['effect']}")
        print(f"      evidence: {r['evidence']}")

    notes = data.get("driving_notes", [])
    if notes:
        print(f"\nDriving notes  : {len(notes)}")
        for n in notes:
            print(f"  * {n['headline']}")
            for line in textwrap.wrap(n["detail"], 66):
                print(f"    {line}")
    print()


def print_result(result, problems):
    print("=" * 72)
    print(f"RACE ENGINEER  ({MODEL})")
    print("=" * 72)
    print(f"\n{result['verdict']}\n")

    changes = result.get("priority_changes", [])
    if changes:
        print("PRIORITY CHANGES")
        for i, c in enumerate(changes, 1):
            arrow = "^" if c["direction"] == "Increase" else "v"
            print(f"\n  {i}. {arrow} {c['parameter']}  ({c['magnitude']}, {c['confidence']} confidence)")
            for line in textwrap.wrap(c["why"], 66):
                print(f"     {line}")
            print(f"     Feel for: {c['what_to_feel']}")
    else:
        print("PRIORITY CHANGES\n  None — the car is not what is limiting you.")

    advice = result.get("driving_advice", [])
    if advice:
        print("\n\nDRIVING")
        for a in advice:
            gain = f"  (~{a['estimated_gain_s']:.2f}s/lap)" if a.get("estimated_gain_s") else ""
            print(f"\n  * {a['focus']}{gain}")
            for line in textwrap.wrap(a["detail"], 66):
                print(f"    {line}")

    print("\n\nTEST PLAN")
    for line in textwrap.wrap(result["test_plan"], 68):
        print(f"  {line}")

    if problems:
        print("\n\n!! VALIDATION FAILURES — these were rejected, not shown above:")
        for p in problems:
            print(f"   - {p}")
    print()


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("analysis_json", help="JSON emitted by: pitwall --analyse X --json Y")
    ap.add_argument("--track", help="track name, for context")
    ap.add_argument("--car", help="car name, for context")
    ap.add_argument("--offline", action="store_true",
                    help="skip the model; print the physics layer's report only")
    args = ap.parse_args()

    try:
        with open(args.analysis_json) as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        print(f"error: could not read {args.analysis_json}: {e}", file=sys.stderr)
        return 1

    if data.get("schema") != "pitwall.analysis.v1":
        print(f"warning: unexpected schema {data.get('schema')!r}", file=sys.stderr)

    if args.offline:
        offline_report(data)
        return 0

    try:
        import anthropic
    except ImportError:
        print("The anthropic SDK is not installed:  pip install anthropic", file=sys.stderr)
        print("Falling back to the offline report.\n", file=sys.stderr)
        offline_report(data)
        return 0

    try:
        client = anthropic.Anthropic()
    except Exception as e:
        print(f"Could not construct a client ({e}).", file=sys.stderr)
        print("Set ANTHROPIC_API_KEY or run `ant auth login`. Offline report:\n", file=sys.stderr)
        offline_report(data)
        return 0

    try:
        # Streaming because adaptive thinking on a reasoning-heavy prompt can
        # run long enough to trip the non-streaming HTTP timeout.
        with client.messages.stream(
            model=MODEL,
            max_tokens=8000,
            system=[{
                "type": "text",
                "text": VEHICLE_DYNAMICS,
                # The physics reference is identical on every run; caching it
                # cuts the input cost of a session to almost nothing.
                "cache_control": {"type": "ephemeral"},
            }],
            thinking={"type": "adaptive"},
            output_config={"format": {"type": "json_schema", "schema": RESPONSE_SCHEMA},
                           "effort": "high"},
            messages=[{"role": "user", "content": build_user_message(data, args.track, args.car)}],
        ) as stream:
            response = stream.get_final_message()
    except anthropic.RateLimitError as e:
        print(f"Rate limited: {e}. Offline report:\n", file=sys.stderr)
        offline_report(data)
        return 0
    except anthropic.APIStatusError as e:
        print(f"API error {e.status_code}: {e.message}. Offline report:\n", file=sys.stderr)
        offline_report(data)
        return 0
    except anthropic.APIConnectionError:
        print("Network unreachable. Offline report:\n", file=sys.stderr)
        offline_report(data)
        return 0

    if response.stop_reason == "refusal":
        print("The model declined this request. Offline report:\n", file=sys.stderr)
        offline_report(data)
        return 0

    text = next((b.text for b in response.content if b.type == "text"), None)
    if not text:
        print("Empty response. Offline report:\n", file=sys.stderr)
        offline_report(data)
        return 0

    result = json.loads(text)
    problems = validate(result, data)
    if problems:
        # A parameter the physics layer never identified is not a suggestion,
        # it is a hallucination. Drop it before the driver sees it.
        allowed = {r["parameter"] for r in data.get("physics_recommendations", [])}
        result["priority_changes"] = [
            c for c in result.get("priority_changes", []) if c.get("parameter") in allowed
        ]

    print_result(result, problems)

    u = response.usage
    print(f"[usage] in={u.input_tokens} out={u.output_tokens} "
          f"cache_read={getattr(u, 'cache_read_input_tokens', 0)} "
          f"cache_write={getattr(u, 'cache_creation_input_tokens', 0)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
