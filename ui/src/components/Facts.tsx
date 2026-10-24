// Facts as the reasoner spells them. An asserted fact sits on a solid chip; a
// derived one on a dashed outline with the rule that concluded it, so an
// inference never reads as something the graph stores.

import { useState } from "react";
import { ChevronRight } from "lucide-react";

import type { AddedFact, Fact } from "../lib/types";
import { cx } from "./ui";

export function factText(f: Fact): string {
  return `${f.s} ${f.p} ${f.o}`;
}

/** One triple; `derived` draws the dashed outline. */
export function FactChip({ fact, derived, className }: { fact: Fact; derived?: boolean; className?: string }) {
  return (
    <code
      className={cx(
        "inline-flex max-w-full min-w-0 flex-wrap items-baseline gap-x-1.5 rounded-input px-1.5 py-0.5 font-mono text-[12px] leading-5 break-all",
        derived ? "border border-dashed border-[color-mix(in_oklch,var(--accent)_55%,transparent)] text-ink" : "bg-sunken text-ink",
        className,
      )}
    >
      <span>{fact.s}</span>
      <span className="text-ink-2">{fact.p}</span>
      <span>{fact.o}</span>
    </code>
  );
}

/** A derived fact with its rule; the premises open on demand. */
export function DerivedFact({ item }: { item: AddedFact }) {
  const [open, setOpen] = useState(false);
  const premises = item.premises ?? [];
  const derived = Boolean(item.rule);
  return (
    <li className="min-w-0">
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <FactChip fact={item.fact} derived={derived} />
        <span className="text-xs text-ink-3">{derived ? <>derived by <span className="font-mono text-ink-2">{item.rule}</span></> : "asserted"}</span>
        {premises.length ? (
          <button
            onClick={() => setOpen((o) => !o)}
            aria-expanded={open}
            className="inline-flex items-center gap-0.5 rounded px-1 text-xs text-ink-3 hover:text-ink"
          >
            <ChevronRight className={cx("size-3 transition-transform duration-150", open && "rotate-90")} aria-hidden />
            {premises.length === 1 ? "1 premise" : `${premises.length} premises`}
          </button>
        ) : null}
      </div>
      {open ? (
        <ol className="mt-1 ml-3 flex flex-col gap-1 border-l border-hairline pl-3">
          {premises.map((p, i) => (
            <li key={i} className="min-w-0">
              <FactChip fact={p} />
            </li>
          ))}
        </ol>
      ) : null}
    </li>
  );
}
