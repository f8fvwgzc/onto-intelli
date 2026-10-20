import { forwardRef, useCallback, useEffect, useImperativeHandle, useMemo, useRef } from "react";
import { forceCenter, forceCollide, forceLink, forceManyBody, forceSimulation, forceX, forceY, type Simulation, type SimulationLinkDatum, type SimulationNodeDatum } from "d3-force";

import type { Edge, Entity } from "../lib/types";

interface SimNode extends SimulationNodeDatum {
  id: string;
  r: number;
}

type SimLink = SimulationLinkDatum<SimNode> & { key: string; rel: string };

export interface GraphCanvasHandle {
  fit: () => void;
  center: (id: string) => void;
  zoomBy: (factor: number) => void;
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
  ariaLabel: string;
}

/** Unordered key of an edge between two entities (highlights ignore direction). */
export function pairKey(a: string, b: string): string {
  return a < b ? `${a}|${b}` : `${b}|${a}`;
}

const reduceMotion = () => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;

function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

export const GraphCanvas = forwardRef<GraphCanvasHandle, GraphCanvasProps>(function GraphCanvas(props, ref) {
  const { nodes, edges, colorOf, isAlert, selected, picked, highlightNodes, highlightEdges, highlightTone = "accent", onSelect, onExpand, ariaLabel } = props;
  const svgRef = useRef<SVGSVGElement>(null);
  const viewRef = useRef<SVGGElement>(null);
  const positions = useRef(new Map<string, SimNode>());
  const nodeEls = useRef(new Map<string, SVGGElement>());
  const edgeEls = useRef(new Map<string, SVGLineElement>());
  const sim = useRef<Simulation<SimNode, SimLink> | null>(null);
  const view = useRef({ k: 1, x: 0, y: 0 });
  const fitted = useRef(false);

  const degree = useMemo(() => {
    const d = new Map<string, number>();
    for (const e of edges) {
      d.set(e.from, (d.get(e.from) ?? 0) + 1);
      d.set(e.to, (d.get(e.to) ?? 0) + 1);
    }
    return d;
  }, [edges]);

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
    for (const [key, el] of edgeEls.current) {
      const [from, , to] = key.split("\u0000");
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
      applyView();
    },
    [applyView],
  );

  useImperativeHandle(
    ref,
    () => ({
      fit: () => void fit(),
      center,
      zoomBy: (factor: number) => {
        const r = svgRef.current?.getBoundingClientRect();
        if (r) zoomAt(factor, r.width / 2, r.height / 2);
      },
    }),
    [fit, center, zoomAt],
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
    const simLinks: SimLink[] = edges
      .filter((e) => present.has(e.from) && present.has(e.to))
      .map((e) => ({ source: e.from, target: e.to, rel: e.rel, key: `${e.from}\u0000${e.rel}\u0000${e.to}` }));

    if (!sim.current) {
      sim.current = forceSimulation<SimNode, SimLink>()
        .force("charge", forceManyBody<SimNode>().strength(-180).distanceMax(420))
        .force("link", forceLink<SimNode, SimLink>().id((d) => d.id).distance(70).strength(0.5))
        .force("collide", forceCollide<SimNode>().radius((d) => d.r + 10))
        .force("x", forceX<SimNode>(0).strength(0.04))
        .force("y", forceY<SimNode>(0).strength(0.04))
        .force("center", forceCenter(0, 0).strength(0.05))
        .on("tick", draw);
    }
    const s = sim.current;
    s.nodes(simNodes);
    (s.force("link") as ReturnType<typeof forceLink<SimNode, SimLink>>).links(simLinks);
    const settle = () => {
      if (!fitted.current && nodes.length && fit()) fitted.current = true;
    };
    if (reduceMotion()) {
      s.stop();
      s.alpha(1);
      for (let i = 0; i < 260; i++) s.tick();
      draw();
      settle();
    } else {
      s.alpha(Math.max(s.alpha(), 0.7)).restart();
      s.on("end.fit", settle);
      // Fit once the first layout has roughly settled.
      const t = window.setTimeout(settle, 700);
      return () => window.clearTimeout(t);
    }
  }, [nodes, edges, degree, draw, fit]);

  useEffect(() => () => void sim.current?.stop(), []);

  // The first fit waits for the canvas to have a size (it may mount hidden).
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => {
      if (!fitted.current && nodes.length && fit()) fitted.current = true;
    });
    ro.observe(svg);
    return () => ro.disconnect();
  }, [fit, nodes.length]);

  // A new data set (workspace, snapshot) refits.
  const signature = nodes.length ? nodes[0].id : "";
  useEffect(() => {
    if (nodes.length === 0) fitted.current = false;
  }, [signature, nodes.length]);

  // Pointer handling: pan the background, drag nodes, pinch and wheel zoom.
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const drag = useRef<{ id?: string; startX: number; startY: number; moved: boolean; lastPinch?: number } | null>(null);

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
      if (p) {
        const v = view.current;
        p.fx = (now.x - v.x) / v.k;
        p.fy = (now.y - v.y) / v.k;
        if (reduceMotion()) {
          p.x = p.fx;
          p.y = p.fy;
          draw();
        } else {
          sim.current?.alphaTarget(0.25).restart();
        }
      }
    } else {
      view.current.x += now.x - prev.x;
      view.current.y += now.y - prev.y;
      applyView();
    }
  };

  // Taps select; a second tap on the same node expands it (pointer capture
  // means native dblclick cannot be relied on, and touch has none).
  const lastTap = useRef<{ id?: string; t: number }>({ t: 0 });
  const onPointerUp = (e: React.PointerEvent<SVGSVGElement>) => {
    pointers.current.delete(e.pointerId);
    const d = drag.current;
    if (pointers.current.size > 0) return;
    drag.current = null;
    sim.current?.alphaTarget(0);
    if (!d || d.moved) return;
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
        .graph-canvas .node:focus-visible { outline: none; }
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
                  if (el) edgeEls.current.set(key, el);
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
            return (
              <g
                key={n.id}
                data-node={n.id}
                className="node cursor-pointer"
                role="button"
                tabIndex={0}
                aria-label={`${n.kind} ${n.title || n.id}${n.state ? `, ${n.state}` : ""}`}
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
                  }
                }}
              >
                <circle className="ring" r={r + 4} fill="none" stroke={isSel ? "var(--text)" : hl ? tone : "transparent"} strokeWidth={isSel ? 2 : 1.6} opacity={isSel || hl ? 1 : 0} />
                <circle r={r} fill={colorOf(n.kind)} stroke="var(--page)" strokeWidth={2} />
                {alert ? <circle r={3} cx={r * 0.72} cy={-r * 0.72} fill="var(--danger)" stroke="var(--page)" strokeWidth={1.5} /> : null}
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
