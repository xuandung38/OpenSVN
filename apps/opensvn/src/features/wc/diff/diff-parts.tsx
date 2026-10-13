import { SafeText } from "../../../components/ui/safe-text";
import type { ImageSides as ImageSidesData, PropChange } from "../../../ipc/bindings";
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

/** Before / after images side by side (plain `<img>`, never inline SVG markup). */
export function ImageSides({
  image,
  before,
  after,
}: {
  image: ImageSidesData;
  before: string;
  after: string;
}) {
  const sides = [
    { label: before, src: image.leftUrl },
    { label: after, src: image.rightUrl },
  ];
  return (
    <div className="grid h-full grid-cols-2 gap-3 p-3">
      {sides.map((s) => (
        <figure key={s.label} className="m-0 flex min-h-0 flex-col items-center gap-1.5">
          <figcaption className="text-[12px] text-muted-text">{s.label}</figcaption>
          {s.src && isSafeImageSrc(s.src) ? (
            <img
              src={s.src}
              alt={s.label}
              className="max-h-full max-w-full rounded-md border border-line object-contain"
            />
          ) : (
            <span className="text-muted-text">-</span>
          )}
        </figure>
      ))}
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
