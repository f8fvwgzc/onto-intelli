import { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useRef, useState } from "react";
import {
  forceCenter,
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  forceX,
  forceY,
  type ForceCollide,
  type ForceLink,
  type ForceManyBody,
  type Simulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from "d3-force";

import type { Edge, Entity } from "../lib/types";
import { DEFAULT_PHYSICS, animates, type Physics } from "./GraphPhysics";

interface SimNode extends SimulationNodeDatum {
  id: string;
  r: number;
}

type SimLink = SimulationLinkDatum<SimNode> & { key: string; rel: string };

export interface GraphCanvasHandle {
  fit: () => void;
  center: (id: string) => void;
  zoomBy: (factor: number) => void;
  unpinAll: () => void;
}

export interface GraphCanvasProps {
  nodes: Entity[];
  edges: Edge[];
  colorOf: (kind: string) => string;
  isAlert?: (e: Entity) => boolean;
  selected?: string;
  picked?: string[];
  highlightNodes?: Set<string>;
  highlightEdges?: Set<string>;
  /** Highlight in the danger colour (root-cause chains) rather than the accent. */
  highlightTone?: "accent" | "danger";
  onSelect: (id: string | undefined) => void;
  onExpand: (id: string) => void;
  /** Forces of the layout; dragging a node pulls its neighbours along through them. */
  physics?: Physics;
  /** How many nodes are pinned in place (Shift-drop, Shift-click or P). */
  onPinsChange?: (count: number) => void;
  ariaLabel: string;
}

/** Unordered key of an edge between two entities (highlights ignore direction). */
export function pairKey(a: string, b: string): string {
  return a < b ? `${a}|${b}` : `${b}|${a}`;
}

function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

export const GraphCanvas = forwardRef<GraphCanvasHandle, GraphCanvasProps>(function GraphCanvas(props, ref) {
  const { nodes, edges, colorOf, isAlert, selected, picked, highlightNodes, highlightEdges, highlightTone = "accent", onSelect, onExpand, physics = DEFAULT_PHYSICS, onPinsChange, ariaLabel } = props;
  const svgRef = useRef<SVGSVGElement>(null);
  const viewRef = useRef<SVGGElement>(null);
  const positions = useRef(new Map<string, SimNode>());
  const nodeEls = useRef(new Map<string, SVGGElement>());
  const edgeEls = useRef(new Map<string, { el: SVGLineElement; from: string; to: string }>());
  const sim = useRef<Simulation<SimNode, SimLink> | null>(null);
  const view = useRef({ k: 1, x: 0, y: 0 });
  /** The first fit is done, or the view was moved before it and it no longer applies. */
  const fitted = useRef(false);
  /** The first layout cooled while the canvas had no size: fit once it has one. */
  const fitWhenSized = useRef(false);

  const degree = useMemo(() => {
    const d = new Map<string, number>();
    for (const e of edges) {
      d.set(e.from, (d.get(e.from) ?? 0) + 1);
      d.set(e.to, (d.get(e.to) ?? 0) + 1);
    }
    return d;
  }, [edges]);
  const degreeRef = useRef(degree);
  degreeRef.current = degree;
  const physicsRef = useRef(physics);
  physicsRef.current = physics;
  const [pinned, setPinned] = useState<ReadonlySet<string>>(new Set());

  const applyView = useCallback(() => {
    const v = view.current;
    viewRef.current?.setAttribute("transform", `translate(${v.x},${v.y}) scale(${v.k})`);
    svgRef.current?.setAttribute("data-far", v.k < 0.75 ? "1" : "0");
  }, []);

  const draw = useCallback(() => {
    for (const [id, el] of nodeEls.current) {
      const p = positions.current.get(id);
      if (p) el.setAttribute("transform", `translate(${p.x ?? 0},${p.y ?? 0})`);
    }
    for (const { el, from, to } of edgeEls.current.values()) {
      const a = positions.current.get(from);
      const b = positions.current.get(to);
      if (!a || !b) continue;
      el.setAttribute("x1", String(a.x ?? 0));
      el.setAttribute("y1", String(a.y ?? 0));
      el.setAttribute("x2", String(b.x ?? 0));
      el.setAttribute("y2", String(b.y ?? 0));
    }
  }, []);

  /** Fit every node in view; false while the canvas has no size yet. */
  const fit = useCallback((): boolean => {
    const svg = svgRef.current;
    if (!svg || positions.current.size === 0) return false;
    const ids = nodes.map((n) => n.id);
    const pts = ids.map((id) => positions.current.get(id)).filter(Boolean) as SimNode[];
    if (!pts.length) return false;
    const xs = pts.map((p) => p.x ?? 0);
    const ys = pts.map((p) => p.y ?? 0);
    const [minX, maxX, minY, maxY] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    const { width, height } = svg.getBoundingClientRect();
    if (width < 40 || height < 40) return false;
    const pad = 60;
    const k = Math.min(2, Math.max(0.15, Math.min((width - pad * 2) / Math.max(1, maxX - minX), (height - pad * 2) / Math.max(1, maxY - minY))));
    view.current = { k, x: width / 2 - ((minX + maxX) / 2) * k, y: height / 2 - ((minY + maxY) / 2) * k };
    applyView();
    return true;
  }, [nodes, applyView]);

  const center = useCallback(
    (id: string) => {
      const svg = svgRef.current;
      const p = positions.current.get(id);
      if (!svg || !p) return;
      const { width, height } = svg.getBoundingClientRect();
      const k = Math.max(view.current.k, 1);
      view.current = { k, x: width / 2 - (p.x ?? 0) * k, y: height / 2 - (p.y ?? 0) * k };
      fitted.current = true;
      applyView();
    },
    [applyView],
  );

  const zoomAt = useCallback(
    (factor: number, cx: number, cy: number) => {
      const v = view.current;
      const k = Math.min(4, Math.max(0.1, v.k * factor));
      const f = k / v.k;
      view.current = { k, x: cx - (cx - v.x) * f, y: cy - (cy - v.y) * f };
      fitted.current = true;
      applyView();
    },
    [applyView],
  );

  /** Settle the layout in one step and draw the result: the layout does not animate. */
  const settleNow = useCallback(
    (s: Simulation<SimNode, SimLink>, alpha: number, ticks: number) => {
      s.stop().alpha(Math.max(s.alpha(), alpha));
      for (let i = 0; i < ticks; i++) s.tick();
      draw();
    },
    [draw],
  );

  /** Wake the layout so a change settles. */
  const stir = useCallback(
    (alpha: number) => {
      const s = sim.current;
      if (!s) return;
      if (animates(physicsRef.current)) s.alpha(Math.max(s.alpha(), alpha)).restart();
      else settleNow(s, alpha, 120);
    },
    [settleNow],
  );

  const markPinned = useCallback((id: string, on: boolean) => {
    setPinned((prev) => {
      if (prev.has(id) === on) return prev;
      const next = new Set(prev);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);

  const togglePin = useCallback(
    (id: string) => {
      const p = positions.current.get(id);
      if (!p) return;
      const on = p.fx == null;
      p.fx = on ? p.x : null;
      p.fy = on ? p.y : null;
      markPinned(id, on);
      if (!on) stir(0.2);
    },
    [markPinned, stir],
  );

  const unpinAll = useCallback(() => {
    for (const p of positions.current.values()) {
      p.fx = null;
      p.fy = null;
    }
    setPinned(new Set());
    stir(0.3);
  }, [stir]);

  useEffect(() => onPinsChange?.(pinned.size), [pinned, onPinsChange]);

  /** Push the physics settings into the simulation's forces. */
  const applyPhysics = useCallback((s: Simulation<SimNode, SimLink>, p: Physics) => {
    const deg = (n: SimNode | string | number) => degreeRef.current.get(typeof n === "object" ? n.id : String(n)) ?? 1;
    s.velocityDecay(p.friction);
    (s.force("charge") as ForceManyBody<SimNode>).strength(-p.repulsion).distanceMax(Math.max(420, p.distance * 6));
    (s.force("link") as ForceLink<SimNode, SimLink>)
      .distance(p.distance)
      // A hub answers each of its links more softly, so dragging one of forty
      // leaves does not haul the hub, and dragging the hub spreads the leaves.
      .strength((l) => Math.min(1, p.follow / Math.sqrt(Math.max(1, Math.min(deg(l.source), deg(l.target))))));
    (s.force("collide") as ForceCollide<SimNode>).radius((d) => d.r + p.collision);
  }, []);

  useImperativeHandle(
    ref,
    () => ({
      fit: () => void fit(),
      center,
      zoomBy: (factor: number) => {
        const r = svgRef.current?.getBoundingClientRect();
        if (r) zoomAt(factor, r.width / 2, r.height / 2);
      },
      unpinAll,
    }),
    [fit, center, zoomAt, unpinAll],
  );

  // Layout: keep known positions, place newcomers next to a neighbour.
  useEffect(() => {
    const known = positions.current;
    const neighbours = new Map<string, string[]>();
    for (const e of edges) {
      neighbours.set(e.from, [...(neighbours.get(e.from) ?? []), e.to]);
      neighbours.set(e.to, [...(neighbours.get(e.to) ?? []), e.from]);
    }
    const simNodes: SimNode[] = nodes.map((n) => {
      const r = Math.min(16, 6 + Math.sqrt(degree.get(n.id) ?? 0) * 2);
      const prev = known.get(n.id);
      if (prev) {
        prev.r = r;
        return prev;
      }
      const anchor = (neighbours.get(n.id) ?? []).map((id) => known.get(id)).find(Boolean);
      const angle = Math.random() * Math.PI * 2;
      const node: SimNode = {
        id: n.id,
        r,
        x: (anchor?.x ?? 0) + Math.cos(angle) * 40,
        y: (anchor?.y ?? 0) + Math.sin(angle) * 40,
      };
      known.set(n.id, node);
      return node;
    });
    const present = new Set(nodes.map((n) => n.id));
    for (const id of [...known.keys()]) if (!present.has(id)) known.delete(id);
    setPinned((prev) => ([...prev].every((id) => present.has(id)) ? prev : new Set([...prev].filter((id) => present.has(id)))));
    const simLinks: SimLink[] = edges
      .filter((e) => present.has(e.from) && present.has(e.to))
      .map((e) => ({ source: e.from, target: e.to, rel: e.rel, key: `${e.from}\u0000${e.rel}\u0000${e.to}` }));

    if (!sim.current) {
      sim.current = forceSimulation<SimNode, SimLink>()
        .force("charge", forceManyBody<SimNode>())
        .force("link", forceLink<SimNode, SimLink>().id((d) => d.id))
        .force("collide", forceCollide<SimNode>())
        .force("x", forceX<SimNode>(0).strength(0.04))
        .force("y", forceY<SimNode>(0).strength(0.04))
        .force("center", forceCenter(0, 0).strength(0.05))
        .on("tick", draw);
      applyPhysics(sim.current, physicsRef.current);
    }
    const s = sim.current;
    s.nodes(simNodes);
    (s.force("link") as ReturnType<typeof forceLink<SimNode, SimLink>>).links(simLinks);
    const settle = () => {
      if (fitted.current || !nodes.length) return;
      fitted.current = fit();
      fitWhenSized.current = !fitted.current;
    };
    if (!animates(physicsRef.current)) {
      settleNow(s, 1, 260);
      settle();
    } else {
      s.alpha(Math.max(s.alpha(), 0.7)).restart();
      s.on("end.fit", settle);
      // Fit once the first layout has roughly settled.
      const t = window.setTimeout(settle, 700);
      return () => window.clearTimeout(t);
    }
  }, [nodes, edges, degree, draw, fit, applyPhysics, settleNow]);

  useEffect(() => () => void sim.current?.stop(), []);

  // Settings apply live and stir the layout so the change shows. Settling in
  // one step runs at most once a frame while a slider moves.
  const appliedPhysics = useRef(physics);
  useEffect(() => {
    const s = sim.current;
    if (!s || appliedPhysics.current === physics) return;
    appliedPhysics.current = physics;
    applyPhysics(s, physics);
    if (animates(physics)) {
      s.alpha(Math.max(s.alpha(), 0.3)).restart();
      return;
    }
    const frame = requestAnimationFrame(() => settleNow(s, 0.3, 80));
    return () => cancelAnimationFrame(frame);
  }, [physics, applyPhysics, settleNow]);

  // A canvas that mounts hidden takes its first fit once it has a size. Until
  // the first fit, the view centres on the origin the layout spreads out from:
  // fitting the unsettled layout would zoom in on a bunch of nodes.
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => {
      if (fitted.current) return;
      if (fitWhenSized.current) {
        fitted.current = fit();
        fitWhenSized.current = !fitted.current;
        return;
      }
      const { width, height } = svg.getBoundingClientRect();
      view.current = { ...view.current, x: width / 2, y: height / 2 };
      applyView();
    });
    ro.observe(svg);
    return () => ro.disconnect();
  }, [fit, applyView]);

  // A new data set (workspace, snapshot) refits.
  const signature = nodes.length ? nodes[0].id : "";
  useEffect(() => {
    if (nodes.length === 0) fitted.current = fitWhenSized.current = false;
  }, [signature, nodes.length]);

  // Pointer handling: pan the background, drag nodes, pinch and wheel zoom.
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const drag = useRef<{ id?: string; startX: number; startY: number; moved: boolean; held?: boolean; lastPinch?: number } | null>(null);

  const local = (e: { clientX: number; clientY: number }) => {
    const r = svgRef.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  const onPointerDown = (e: React.PointerEvent<SVGSVGElement>) => {
    const target = (e.target as Element).closest("[data-node]");
    pointers.current.set(e.pointerId, local(e));
    svgRef.current?.setPointerCapture(e.pointerId);
    drag.current = { id: target?.getAttribute("data-node") ?? undefined, startX: e.clientX, startY: e.clientY, moved: false };
  };

  const onPointerMove = (e: React.PointerEvent<SVGSVGElement>) => {
    if (!pointers.current.has(e.pointerId)) return;
    const prev = pointers.current.get(e.pointerId)!;
    const now = local(e);
    pointers.current.set(e.pointerId, now);
    const d = drag.current;
    if (!d) return;
    if (pointers.current.size === 2) {
      const [a, b] = [...pointers.current.values()];
      const dist = Math.hypot(a.x - b.x, a.y - b.y);
      if (d.lastPinch) zoomAt(dist / d.lastPinch, (a.x + b.x) / 2, (a.y + b.y) / 2);
      d.lastPinch = dist;
      d.moved = true;
      return;
    }
    if (Math.abs(e.clientX - d.startX) + Math.abs(e.clientY - d.startY) > 4) d.moved = true;
    if (!d.moved) return;
    if (d.id) {
      const p = positions.current.get(d.id);
      const s = sim.current;
      if (p && s) {
        if (d.held === undefined) {
          // Drag start: the node follows the pointer, and the warmed layout lets
          // its links and charge pull the neighbours along and spread them out.
          // This animates even when the layout does not: the motion is the user's own.
          d.held = p.fx != null;
          fitted.current = true;
          s.alphaTarget(0.3).alpha(Math.max(s.alpha(), 0.3)).restart();
        }
        const v = view.current;
        p.fx = (now.x - v.x) / v.k;
        p.fy = (now.y - v.y) / v.k;
      }
    } else {
      view.current.x += now.x - prev.x;
      view.current.y += now.y - prev.y;
      applyView();
    }
  };

  /** Drag end: the layout cools, and the node rejoins it unless it stays pinned. */
  const release = (id: string, keep: boolean) => {
    const s = sim.current;
    const p = positions.current.get(id);
    if (!s || !p) return;
    p.x = p.fx ?? p.x;
    p.y = p.fy ?? p.y;
    if (!keep) {
      p.fx = null;
      p.fy = null;
    }
    markPinned(id, keep);
    s.alphaTarget(0);
    if (!animates(physicsRef.current)) settleNow(s, 0, 120);
  };

  // Taps select; a second tap on the same node expands it (pointer capture
  // means native dblclick cannot be relied on, and touch has none).
  const lastTap = useRef<{ id?: string; t: number }>({ t: 0 });
  const onPointerUp = (e: React.PointerEvent<SVGSVGElement>) => {
    pointers.current.delete(e.pointerId);
    const d = drag.current;
    if (pointers.current.size > 0) return;
    drag.current = null;
    if (d?.id && d.held !== undefined) release(d.id, d.held || e.shiftKey);
    if (!d || d.moved) return;
    if (d.id && e.shiftKey) {
      togglePin(d.id);
      return;
    }
    const now = performance.now();
    if (d.id && lastTap.current.id === d.id && now - lastTap.current.t < 330) {
      lastTap.current = { t: 0 };
      onExpand(d.id);
      return;
    }
    lastTap.current = { id: d.id, t: now };
    onSelect(d.id);
  };

  const onWheel = (e: React.WheelEvent<SVGSVGElement>) => {
    const p = local(e);
    zoomAt(Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0018)), p.x, p.y);
  };

  // Wheel must be non-passive to stop the page from scrolling.
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg) return;
    const stop = (e: WheelEvent) => e.preventDefault();
    svg.addEventListener("wheel", stop, { passive: false });
    return () => svg.removeEventListener("wheel", stop);
  }, []);

  const pickedSet = useMemo(() => new Set(picked ?? []), [picked]);
  const anyHighlight = (highlightNodes?.size ?? 0) > 0;
  const tone = highlightTone === "danger" ? "var(--danger)" : "var(--accent)";

  return (
    <svg
      ref={svgRef}
      role="group"
      aria-label={ariaLabel}
      className="graph-canvas size-full touch-none select-none"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onWheel={onWheel}
    >
      <style>{`
        .graph-canvas[data-far="1"] .lbl-minor { display: none; }
        .graph-canvas .node:focus { outline: none; }
        .graph-canvas .node:focus-visible .ring { stroke: var(--accent); stroke-width: 2; opacity: 1; }
      `}</style>
      <g ref={viewRef}>
        <g>
          {edges.map((e) => {
            const key = `${e.from}\u0000${e.rel}\u0000${e.to}`;
            const hl = highlightEdges?.has(pairKey(e.from, e.to));
            return (
              <line
                key={key}
                ref={(el) => {
                  if (el) edgeEls.current.set(key, { el, from: e.from, to: e.to });
                  else edgeEls.current.delete(key);
                }}
                stroke={hl ? tone : "var(--gray-3)"}
                strokeWidth={hl ? 2.2 : 1}
                strokeOpacity={anyHighlight && !hl ? 0.35 : 1}
              >
                <title>{e.rel}</title>
              </line>
            );
          })}
        </g>
        <g>
          {nodes.map((n) => {
            const r = Math.min(16, 6 + Math.sqrt(degree.get(n.id) ?? 0) * 2);
            const isSel = n.id === selected || pickedSet.has(n.id);
            const hl = highlightNodes?.has(n.id);
            const faded = anyHighlight && !hl && !isSel;
            const major = isSel || hl || (degree.get(n.id) ?? 0) >= 4;
            const alert = isAlert?.(n);
            const pin = pinned.has(n.id);
            return (
              <g
                key={n.id}
                data-node={n.id}
                className="node cursor-pointer"
                role="button"
                tabIndex={0}
                aria-label={`${n.kind} ${n.title || n.id}${n.state ? `, ${n.state}` : ""}${pin ? ", pinned" : ""}`}
                aria-pressed={isSel}
                opacity={faded ? 0.32 : 1}
                ref={(el) => {
                  if (el) nodeEls.current.set(n.id, el);
                  else nodeEls.current.delete(n.id);
                }}
                onKeyDown={(ev) => {
                  if (ev.key === "Enter" || ev.key === " ") {
                    ev.preventDefault();
                    onSelect(n.id);
                  } else if (ev.key === "e") {
                    onExpand(n.id);
                  } else if (ev.key === "p") {
                    togglePin(n.id);
                  }
                }}
              >
                <circle className="ring" r={r + 4} fill="none" stroke={isSel ? "var(--text)" : hl ? tone : "transparent"} strokeWidth={isSel ? 2 : 1.6} opacity={isSel || hl ? 1 : 0} />
                <circle r={r} fill={colorOf(n.kind)} stroke="var(--page)" strokeWidth={2} />
                {alert ? <circle r={3} cx={r * 0.72} cy={-r * 0.72} fill="var(--danger)" stroke="var(--page)" strokeWidth={1.5} /> : null}
                {pin ? <circle r={3} cx={-r * 0.72} cy={-r * 0.72} fill="var(--text)" stroke="var(--page)" strokeWidth={1.5} /> : null}
                <text
                  y={r + 13}
                  textAnchor="middle"
                  className={major ? "" : "lbl-minor"}
                  fontSize={11}
                  fontWeight={isSel ? 600 : 450}
                  fill={isSel ? "var(--text)" : "var(--text-2)"}
                  paintOrder="stroke"
                  stroke="var(--canvas)"
                  strokeWidth={3}
                  strokeLinejoin="round"
                >
                  {truncate(n.title || n.id, 24)}
                </text>
              </g>
            );
          })}
        </g>
      </g>
    </svg>
  );
});
