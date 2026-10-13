import { Boxes, Plus } from "lucide-react";
import { type ReactNode, useState } from "react";
import { FileRow } from "../components/list/file-row";
import { RevisionRow } from "../components/list/revision-row";
import { ActivityRow } from "../components/ui/activity-row";
import { Banner } from "../components/ui/banner";
import { Button } from "../components/ui/button";
import { Chip } from "../components/ui/chip";
import { ContextMenu } from "../components/ui/context-menu";
import { BadBox, Dialog, WarnBox } from "../components/ui/dialog";
import { DropdownMenu } from "../components/ui/dropdown-menu";
import { EmptyState } from "../components/ui/empty-state";
import { Input, Select, Textarea } from "../components/ui/fields";
import { LockBadge } from "../components/ui/lock-badge";
import { Popover, PopoverGroup } from "../components/ui/popover";
import { SafeLink } from "../components/ui/safe-link";
import { SegmentedControl } from "../components/ui/segmented-control";
import { Sheet } from "../components/ui/sheet";
import { StatusBar, StatusBarItem } from "../components/ui/status-bar";
import { StatusGlyph } from "../components/ui/status-glyph";
import { Tabs } from "../components/ui/tabs";
import { useToast } from "../components/ui/toast";
import { ToolbarItem } from "../components/ui/toolbar-item";
import type { StatusCode } from "../lib/status";
import { RemoteDemo } from "./remote-demo";

const CODES: StatusCode[] = ["M", "A", "D", "R", "?", "!", "C"];

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2 border-b border-line pb-4">
      <h2 className="m-0 text-[10px] font-medium tracking-[.07em] text-muted-text uppercase">
        {title}
      </h2>
      {children}
    </section>
  );
}

function AllComponents() {
  const [tab, setTab] = useState("changes");
  const [seg, setSeg] = useState("split");
  const [chip, setChip] = useState(true);
  const [dialog, setDialog] = useState(false);
  const [sheet, setSheet] = useState(false);
  const toast = useToast();
  return (
    <div className="flex flex-col gap-4 p-4">
      <Section title="Button">
        <div className="flex gap-2">
          <Button>Default</Button>
          <Button variant="primary">Primary</Button>
          <Button variant="danger">Danger</Button>
          <Button disabled>Disabled</Button>
          <Button small>Small</Button>
        </div>
      </Section>
      <Section title="Toolbar item">
        <div className="flex border border-line">
          <ToolbarItem icon={<Boxes size={18} />} label="Working copy" value="game-client ▾" />
          <ToolbarItem icon="↻" label="Update" value="Update" badge="↓3" className="min-w-0" />
          <ToolbarItem icon="↻" label="Update" value="Updating" busy className="min-w-0" />
        </div>
      </Section>
      <Section title="Tabs · Segmented · Chip">
        <Tabs
          label="Gallery tabs"
          value={tab}
          onValueChange={setTab}
          tabs={[
            { id: "changes", label: "Changes", count: 12 },
            { id: "history", label: "History" },
          ]}
        />
        <div className="flex items-center gap-3">
          <SegmentedControl
            label="Diff mode"
            value={seg}
            onChange={setSeg}
            options={[
              { value: "split", label: "Split" },
              { value: "unified", label: "Unified" },
            ]}
          />
          <Chip on={chip} onClick={() => setChip((v) => !v)}>
            Conflicts
          </Chip>
          <Chip>Locked</Chip>
        </div>
      </Section>
      <Section title="Input · Select · Textarea">
        <div className="grid grid-cols-3 gap-2">
          <Input aria-label="Input" placeholder="Input" />
          <Select aria-label="Select" defaultValue="a">
            <option value="a">Option A</option>
            <option value="b">Option B</option>
          </Select>
          <Textarea aria-label="Textarea" rows={2} placeholder="Textarea" />
        </div>
      </Section>
      <Section title="Status glyph · Lock badge">
        <div className="flex items-center gap-3">
          {CODES.map((c) => (
            <StatusGlyph key={c} code={c} />
          ))}
          <LockBadge owner="me" />
          <LockBadge owner="other" />
        </div>
      </Section>
      <Section title="File row · Revision row">
        <div className="h-[28px]">
          <FileRow path="src/engine/renderer.ts" status="M" checked />
        </div>
        <div className="h-[28px]">
          <FileRow path="assets/textures/hero.png" status="M" selected lock="other" />
        </div>
        <div className="h-[28px]">
          <FileRow path="src/ui/main-menu.tsx" status="C" />
        </div>
        <div className="h-[44px]">
          <RevisionRow
            rev={48213}
            message="Fix shadow flicker"
            author="binh"
            when="2h ago"
            incoming
          />
        </div>
        <div className="h-[44px]">
          <RevisionRow rev={48210} message="Update module" author="chi" when="1d ago" selected />
        </div>
      </Section>
      <Section title="Banner">
        <Banner title="2 conflicts" actions={<Button small>Resolve</Button>}>
          Files with conflicts cannot be committed until resolved.
        </Banner>
        <Banner
          tone="danger"
          title="Lock stolen"
          actions={
            <Button small variant="danger">
              Review
            </Button>
          }
        >
          Another user took your lock on hero.png.
        </Banner>
      </Section>
      <Section title="Menus · Popover">
        <div className="flex gap-2">
          <DropdownMenu
            trigger={<Button>Dropdown</Button>}
            entries={[
              { label: "Commit…", shortcut: "Ctrl+Enter" },
              { label: "Update", shortcut: "Ctrl+U" },
              "separator",
              { label: "Revert", disabled: true },
            ]}
          />
          <ContextMenu
            entries={[{ label: "Show in file manager" }, "separator", { label: "Copy path" }]}
          >
            <div className="rounded-md border border-dashed border-line px-3 py-1.5 text-muted-text">
              Right-click here
            </div>
          </ContextMenu>
          <Popover trigger={<Button>Popover</Button>}>
            <Input aria-label="Search" placeholder="Search" />
            <PopoverGroup>Recent</PopoverGroup>
            <Button>game-client</Button>
          </Popover>
        </div>
      </Section>
      <Section title="Dialog · Sheet · Toast">
        <div className="flex gap-2">
          <Button onClick={() => setDialog(true)}>Open dialog</Button>
          <Button onClick={() => setSheet(true)}>Open sheet</Button>
          <Button onClick={() => toast.show("Committed 3 files · r48215")}>Show toast</Button>
        </div>
        <Dialog
          open={dialog}
          onClose={() => setDialog(false)}
          title="Discard changes"
          footer={
            <>
              <Button onClick={() => setDialog(false)}>Cancel</Button>
              <Button variant="danger" onClick={() => setDialog(false)}>
                Discard
              </Button>
            </>
          }
        >
          <WarnBox>3 files will be reverted.</WarnBox>
          <BadBox>This cannot be undone.</BadBox>
          <Input aria-label="Reason" placeholder="Reason" />
        </Dialog>
        <Sheet
          open={sheet}
          onClose={() => setSheet(false)}
          title="Settings"
          footer={<Button onClick={() => setSheet(false)}>Close</Button>}
        >
          <p className="m-4">Sheet body</p>
        </Sheet>
      </Section>
      <Section title="Activity row">
        <ActivityRow
          title="Update to HEAD"
          percent={62}
          state="running"
          onCancel={() => undefined}
        />
        <ActivityRow title="Commit" percent={100} state="done" />
        <ActivityRow title="Update: network error" percent={30} state="failed" />
      </Section>
      <Section title="Status bar · Empty state · Safe link">
        <StatusBar>
          <StatusBarItem tone="ok">Connected</StatusBarItem>
          <StatusBarItem>r48210</StatusBarItem>
          <StatusBarItem tone="bad">Offline</StatusBarItem>
        </StatusBar>
        <div className="h-40 rounded-md border border-line">
          <EmptyState
            title="No working copy"
            description="Add an existing working copy or check out a repository."
            actions={
              <>
                <Button variant="primary">
                  <Plus size={14} /> Add existing
                </Button>
                <Button>Checkout</Button>
              </>
            }
          />
        </div>
        <SafeLink href="https://example.com/r/48210">Open revision on web</SafeLink>
      </Section>
    </div>
  );
}

/** Dev-only `/__gallery`: every base component, light and dark side by side. Not a CI gate. */
export function GalleryPage() {
  return (
    <div className="h-full overflow-auto">
      <RemoteDemo />
      <div className="grid min-w-[1100px] grid-cols-2">
        <div data-theme="light" className="bg-bg text-fg">
          <AllComponents />
        </div>
        <div data-theme="dark" className="bg-bg text-fg">
          <AllComponents />
        </div>
      </div>
    </div>
  );
}
