import type { RemotePlatform } from '@/lib/types';

const PLATFORM_META: Record<RemotePlatform, {
  label: string;
  short: string;
  className: string;
}> = {
  windows: {
    label: 'Windows',
    short: 'Win',
    className: 'border-sky-500/30 bg-sky-500/10 text-sky-300',
  },
  linux: {
    label: 'Linux',
    short: 'Linux',
    className: 'border-amber-500/30 bg-amber-500/10 text-amber-300',
  },
  macos: {
    label: 'macOS',
    short: 'macOS',
    className: 'border-zinc-500/30 bg-zinc-500/10 text-zinc-300',
  },
  unix: {
    label: 'Unix',
    short: 'Unix',
    className: 'border-zinc-500/30 bg-zinc-500/10 text-zinc-300',
  },
};

/** Compact, color-coded tag for the platform detected on a remote connection. */
export default function RemotePlatformBadge({
  platform,
  compact = false,
}: {
  platform?: RemotePlatform;
  compact?: boolean;
}) {
  if (!platform) return null;
  const meta = PLATFORM_META[platform];
  return (
    <span
      className={`inline-flex flex-shrink-0 items-center gap-1 rounded border px-1.5 py-px text-[10px] font-medium leading-4 ${meta.className}`}
      title={`远端系统：${meta.label}`}
    >
      <span className="h-1 w-1 rounded-full bg-current opacity-80" aria-hidden="true" />
      {compact ? meta.short : meta.label}
    </span>
  );
}
