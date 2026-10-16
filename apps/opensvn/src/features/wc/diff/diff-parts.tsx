import { useState } from "react";
import { SafeText } from "../../../components/ui/safe-text";
import { SegmentedControl } from "../../../components/ui/segmented-control";
import type { ImageSides as ImageSidesData, PropChange } from "../../../ipc/bindings";
import { cn } from "../../../lib/cn";
import { useT } from "../../../lib/i18n/i18n";
import { dialogStrings } from "../dialogs/dialog-strings";

/** Only backend asset URLs and inline raster/SVG data may feed an `<img>`; nothing else is loaded. */
export function isSafeImageSrc(src: string): boolean {
  return (
    /^asset:/i.test(src) ||
    /^https?:\/\/asset\.localhost\//i.test(src) ||
    /^data:image\/(png|jpe?g|gif|webp|svg\+xml)[;,]/i.test(src)
  );
}

export type ImageMode = "side" | "onion" | "swipe";

/**
 * Before / after images (doc 01 3.3): side by side, onion skin (the new image over the old one
 * with an opacity slider) or swipe (a slider reveals the new image from the left). The sliders are
 * native range inputs, so arrows / Page Up / Page Down / Home / End work. Only plain `<img>`
 * elements with safe sources are rendered, never inline SVG markup. Overlay modes need both sides.
 */
export function ImageSides({
  image,
  before,
  after,
}: {
  image: ImageSidesData;
  before: string;
  after: string;
}) {
  const d = useT(dialogStrings);
  const [mode, setMode] = useState<ImageMode>("side");
  const [opacity, setOpacity] = useState(50);
  const [swipe, setSwipe] = useState(50);
  const left = image.leftUrl && isSafeImageSrc(image.leftUrl) ? image.leftUrl : null;
  const right = image.rightUrl && isSafeImageSrc(image.rightUrl) ? image.rightUrl : null;
  const both = left !== null && right !== null;
  const shown: ImageMode = both ? mode : "side";
  const sides = [
    { label: before, src: left },
    { label: after, src: right },
  ];
  const img = "max-h-full max-w-full rounded-md border border-line object-contain";

  return (
    <div className="flex h-full min-h-0 flex-col gap-2 p-3">
      <div className="flex flex-wrap items-center gap-3">
        {both ? (
          <SegmentedControl<ImageMode>
            label={d("imageMode")}
            value={shown}
            onChange={setMode}
            options={[
              { value: "side", label: d("imageSideBySide") },
              { value: "onion", label: d("imageOnion") },
              { value: "swipe", label: d("imageSwipe") },
            ]}
          />
        ) : (
          <span className="text-[12px] text-muted-text">{d("imageOneSide")}</span>
        )}
        {shown === "onion" && (
          <label className="flex items-center gap-2 text-[12px]">
            {d("imageOpacity")}
            <input
              type="range"
              min={0}
              max={100}
              step={5}
              value={opacity}
              aria-valuetext={`${opacity}%`}
              onChange={(e) => setOpacity(Number(e.target.value))}
              className="accent-[var(--accent)]"
            />
          </label>
        )}
        {shown === "swipe" && (
          <label className="flex items-center gap-2 text-[12px]">
            {d("imageSwipePos")}
            <input
              type="range"
              min={0}
              max={100}
              step={5}
              value={swipe}
              aria-valuetext={`${swipe}%`}
              onChange={(e) => setSwipe(Number(e.target.value))}
              className="accent-[var(--accent)]"
            />
          </label>
        )}
      </div>
      {shown === "side" ? (
        <div className="grid min-h-0 flex-1 grid-cols-2 gap-3">
          {sides.map((s) => (
            <figure key={s.label} className="m-0 flex min-h-0 flex-col items-center gap-1.5">
              <figcaption className="text-[12px] text-muted-text">{s.label}</figcaption>
              {s.src ? (
                <img src={s.src} alt={s.label} className={img} />
              ) : (
                <span className="text-muted-text">-</span>
              )}
            </figure>
          ))}
        </div>
      ) : (
        <figure className="m-0 flex min-h-0 flex-1 flex-col items-center gap-1.5">
          <figcaption className="text-[12px] text-muted-text">{`${before} / ${after}`}</figcaption>
          {/* Both images share one grid cell, so the new one lies exactly over the old one. */}
          <div className="grid min-h-0 max-w-full place-items-center">
            <img
              src={left ?? undefined}
              alt={before}
              className={cn(img, "col-start-1 row-start-1")}
            />
            <img
              src={right ?? undefined}
              alt={after}
              data-overlay={shown}
              className={cn(img, "col-start-1 row-start-1")}
              style={
                shown === "onion"
                  ? { opacity: opacity / 100 }
                  : { clipPath: `inset(0 ${100 - swipe}% 0 0)` }
              }
            />
          </div>
        </figure>
      )}
    </div>
  );
}

/** Property changes under the content diff. Names and values render as text. */
export function PropChanges({ changes }: { changes: PropChange[] }) {
  const d = useT(dialogStrings);
  if (changes.length === 0) return null;
  return (
    <section aria-label={d("propChanges")} className="border-t border-line bg-surface-2 px-3 py-2">
      <b className="text-[12px]">{d("propChanges")}</b>
      <ul className="m-0 mt-1 flex list-none flex-col gap-1 p-0 font-mono text-[12px]">
        {changes.map((c) => (
          <li key={c.name}>
            <SafeText className="font-semibold">{c.name}</SafeText>
            {c.before !== null && (
              <SafeText as="div" className="whitespace-pre-wrap bg-del-bg px-1">
                {`- ${c.before}`}
              </SafeText>
            )}
            {c.after !== null && (
              <SafeText as="div" className="whitespace-pre-wrap bg-add-bg px-1">
                {`+ ${c.after}`}
              </SafeText>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}
