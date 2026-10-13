import { EmptyState } from "../components/ui/empty-state";

/** Full-window error shown when the app cannot start (e.g. IPC bridge missing in a release build). */
export function FatalError({ message }: { message: string }) {
  return (
    <div role="alert" className="h-full">
      <EmptyState title="OpenSVN cannot start" description={message} />
    </div>
  );
}
