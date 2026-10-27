import { useEffect, useState, useRef, useCallback, useMemo } from 'react';
import { createPortal } from 'react-dom';
import { sshListNetwork } from '@/lib/tauri';
import type { RemoteNetworkInterface, RemoteSocketInfo } from '@/lib/remoteSystem';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { getErrorMessage } from '@/lib/errors';

type SocketInfo = RemoteSocketInfo;
type NetInterface = RemoteNetworkInterface;

type SortKey = 'proto' | 'laddr' | 'lport' | 'raddr' | 'rport' | 'state' | 'pid' | 'process';
type SortDir = 'asc' | 'desc';
type SubView = 'listen' | 'connections' | 'interfaces';

interface NetworkPanelProps {
  sessionId: string;
}

function stateBadge(state: string) {
  const s = state.toUpperCase();
  const base = 'inline-flex px-1.5 py-px rounded text-[10px] font-medium leading-4';
  if (s === 'LISTEN') return <span className={`${base} bg-emerald-500/15 text-emerald-400`}>{state}</span>;
  if (s === 'ESTABLISHED' || s === 'ESTAB') return <span className={`${base} bg-cyan-500/15 text-cyan-400`}>{state}</span>;
  if (s === 'TIME_WAIT' || s === 'CLOSE_WAIT' || s === 'FIN_WAIT1' || s === 'FIN_WAIT2' || s === 'CLOSING' || s === 'SYN_RECV' || s === 'SYN-SENT') return <span className={`${base} bg-amber-500/15 text-amber-400`}>{state}</span>;
  return <span className={`${base} bg-zinc-500/15 text-zinc-400`}>{state}</span>;
}

export default function NetworkPanel({ sessionId }: NetworkPanelProps) {
  const [sockets, setSockets] = useState<SocketInfo[]>([]);
  const [interfaces, setInterfaces] = useState<NetInterface[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [search, setSearch] = useState('');
  const [sortKey, setSortKey] = useState<SortKey>('state');
  const [sortDir, setSortDir] = useState<SortDir>('asc');
  const [subView, setSubView] = useState<SubView>('listen');
  const [menuRow, setMenuRow] = useState<SocketInfo | null>(null);
  const [menuPos, setMenuPos] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const menuRef = useRef<HTMLDivElement>(null);

  const loadData = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const snapshot = await sshListNetwork(sessionId);
      setSockets(snapshot.sockets);
      setInterfaces(snapshot.interfaces);
    } catch (err) {
      setError(`加载失败：${getErrorMessage(err)}`);
    } finally {
      setLoading(false);
    }
  }, [sessionId]);

  useEffect(() => { loadData(); }, [loadData]);

  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuRow(null);
    };
    document.addEventListener('mousedown', handleClickOutside);
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, []);

  const handleContextMenu = (e: React.MouseEvent, row: SocketInfo) => {
    e.preventDefault();
    setMenuRow(row);
    setMenuPos({ x: e.clientX, y: e.clientY });
  };

  const handleCopyInfo = async (row: SocketInfo) => {
    const addr = row.state === 'LISTEN'
      ? `${row.proto} ${row.laddr}:${row.lport}`
      : `${row.proto} ${row.laddr}:${row.lport} → ${row.raddr}:${row.rport}`;
    await writeText(`${addr} ${row.state} PID:${row.pid || '-'} ${row.process}`).catch(() => {});
    setMenuRow(null);
  };

  const toggleSort = (key: SortKey) => {
    if (sortKey === key) setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'));
    else { setSortKey(key); setSortDir('asc'); }
  };

  const SortIcon = ({ active, dir }: { active: boolean; dir: SortDir }) => (
    <span className={`text-[10px] ml-0.5 ${active ? 'text-indigo-400' : 'text-zinc-600'}`}>
      {active ? (dir === 'asc' ? '↑' : '↓') : '↕'}
    </span>
  );

  const filteredAndSorted = useMemo(() => {
    const isListen = subView === 'listen';
    let list = sockets.filter((s) => isListen ? s.state === 'LISTEN' : s.state !== 'LISTEN');
    if (search) {
      const q = search.toLowerCase();
      list = list.filter((s) =>
        s.proto.includes(q) || s.laddr.toLowerCase().includes(q) || String(s.lport).includes(q) ||
        s.raddr.toLowerCase().includes(q) || String(s.rport).includes(q) ||
        s.state.toLowerCase().includes(q) || s.process.toLowerCase().includes(q)
      );
    }
    list.sort((a, b) => {
      let va: any = a[sortKey], vb: any = b[sortKey];
      if (typeof va === 'string') va = va.toLowerCase();
      if (typeof vb === 'string') vb = vb.toLowerCase();
      return (va > vb ? 1 : va < vb ? -1 : 0) * (sortDir === 'asc' ? 1 : -1);
    });
    return list;
  }, [sockets, subView, search, sortKey, sortDir]);

  const renderSocketTable = () => {
    const isListen = subView === 'listen';
    const columns: { key: SortKey; label: string; w: string }[] = isListen
      ? [
          { key: 'proto', label: '协议', w: 'w-12' },
          { key: 'laddr', label: '监听地址', w: '' },
          { key: 'lport', label: '端口', w: 'w-14' },
          { key: 'state', label: '状态', w: 'w-20' },
          { key: 'pid', label: 'PID', w: 'w-16' },
          { key: 'process', label: '进程', w: '' },
        ]
      : [
          { key: 'proto', label: '协议', w: 'w-12' },
          { key: 'laddr', label: '本地地址', w: '' },
          { key: 'lport', label: '本地端口', w: 'w-14' },
          { key: 'raddr', label: '远端地址', w: '' },
          { key: 'rport', label: '远端端口', w: 'w-14' },
          { key: 'state', label: '状态', w: 'w-20' },
          { key: 'pid', label: 'PID', w: 'w-16' },
          { key: 'process', label: '进程', w: '' },
        ];

    return (
      <table className="w-full text-xs table-fixed">
        <thead className="sticky top-0 bg-zinc-900 border-b border-edge-subtle">
          <tr className="text-zinc-500">
            {columns.map((col) => (
              <th key={col.key} className={`px-3 py-2 text-left font-medium whitespace-nowrap ${col.w}`}>
                <button type="button" onClick={() => toggleSort(col.key)} className="flex items-center gap-1 hover:text-zinc-300 transition-colors">
                  {col.label} <SortIcon active={sortKey === col.key} dir={sortDir} />
                </button>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {loading ? (
            <tr>
              <td colSpan={columns.length} className="py-8 text-center">
                <div className="flex items-center justify-center gap-2 text-xs text-zinc-400">
                  <svg className="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24">
                    <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                    <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
                  </svg>
                  <span>加载中...</span>
                </div>
              </td>
            </tr>
          ) : filteredAndSorted.map((row, i) => (
            <tr
              key={`${row.proto}-${row.laddr}-${row.lport}-${row.raddr}-${row.rport}-${i}`}
              className="border-b border-edge-subtle/50 hover:bg-state-soft/50 transition-colors cursor-pointer"
              onContextMenu={(e) => handleContextMenu(e, row)}
            >
              <td className="px-3 py-1.5 font-mono text-[11px] text-cyan-400">{row.proto.toUpperCase()}</td>
              <td className="px-3 py-1.5 font-mono text-[11px] text-zinc-300">{row.laddr}</td>
              <td className="px-3 py-1.5 font-mono text-[11px] text-indigo-400">{row.lport}</td>
              {!isListen && (
                <>
                  <td className="px-3 py-1.5 font-mono text-[11px] text-zinc-300">{row.raddr || '*'}</td>
                  <td className="px-3 py-1.5 font-mono text-[11px] text-zinc-400">{row.rport > 0 ? row.rport : '*'}</td>
                </>
              )}
              <td className="px-3 py-1.5">{stateBadge(row.state)}</td>
              <td className="px-3 py-1.5 font-mono text-[11px] text-zinc-500">{row.pid > 0 ? row.pid : '-'}</td>
              <td className="px-3 py-1.5 text-zinc-200 truncate max-w-[200px]" title={row.process}>{row.process}</td>
            </tr>
          ))}
          {!loading && filteredAndSorted.length === 0 && (
            <tr>
              <td colSpan={columns.length} className="py-8 text-center text-xs text-zinc-500">
                {search ? '无匹配的网络连接' : isListen ? '暂无监听端口' : '暂无活动连接'}
              </td>
            </tr>
          )}
        </tbody>
      </table>
    );
  };

  const renderInterfaces = () => (
    <div className="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-3 gap-2.5 p-3">
      {interfaces.map((iface) => (
        <div key={iface.name} className="rounded-lg border border-edge-subtle bg-zinc-900/50 p-3.5 hover:border-edge/80 transition-colors">
          <div className="flex items-center justify-between mb-2">
            <span className="text-sm font-semibold text-zinc-100">{iface.name}</span>
            <span className={`inline-flex px-1.5 py-px rounded text-[10px] font-medium leading-4 ${iface.state === 'UP' ? 'bg-emerald-500/15 text-emerald-400' : 'bg-red-500/15 text-red-400'}`}>
              {iface.state}
            </span>
          </div>
          <div className="space-y-1">
            <div className="flex justify-between text-xs">
              <span className="text-zinc-500">IP</span>
              <span className="font-mono text-[11px] text-zinc-300">{iface.ip}</span>
            </div>
            {iface.mac !== '-' && (
              <div className="flex justify-between text-xs">
                <span className="text-zinc-500">MAC</span>
                <span className="font-mono text-[11px] text-zinc-400">{iface.mac}</span>
              </div>
            )}
          </div>
        </div>
      ))}
      {interfaces.length === 0 && !loading && (
        <div className="col-span-full py-8 text-center text-xs text-zinc-500">暂无网卡数据</div>
      )}
    </div>
  );

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center gap-2 border-b border-edge-subtle px-3 py-2">
        <input
          type="text"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder="搜索地址、端口或进程…"
          className="flex-1 rounded-md bg-zinc-800 border border-edge px-2 py-1 text-xs text-zinc-100 outline-none focus:border-indigo-500 placeholder:text-zinc-500"
        />
        <button
          type="button"
          onClick={loadData}
          disabled={loading}
          className="inline-flex items-center gap-1.5 rounded-md bg-zinc-800 px-2 py-1 text-xs text-zinc-200 hover:bg-state-mid disabled:opacity-40"
        >
          {loading && (
            <svg className="w-3 h-3 animate-spin" fill="none" viewBox="0 0 24 24">
              <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
              <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z" />
            </svg>
          )}
          {loading ? '刷新中...' : '刷新'}
        </button>
      </div>

      <div className="flex border-b border-edge-subtle px-3">
        {(['listen', 'connections', 'interfaces'] as SubView[]).map((v) => (
          <button
            key={v}
            type="button"
            onClick={() => { setSubView(v); setSortKey('state'); setSortDir('asc'); }}
            className={`px-3 py-1.5 text-xs border-b-2 -mb-px transition-colors ${
              subView === v ? 'border-indigo-500 text-indigo-400' : 'border-transparent text-zinc-500 hover:text-zinc-300'
            }`}
          >
            {{ listen: '监听端口', connections: '活动连接', interfaces: '网卡' }[v]}
          </button>
        ))}
      </div>

      <div className="flex-1 overflow-auto">
        {error && <div className="px-3 py-2 text-xs text-red-300">{error}</div>}
        {subView === 'interfaces' ? renderInterfaces() : renderSocketTable()}
      </div>

      {menuRow && createPortal(
        <div ref={menuRef} className="fixed z-[100] min-w-[140px] rounded-lg border border-edge bg-zinc-800 p-1 shadow-lg" style={{ top: menuPos.y, left: menuPos.x }}>
          <button type="button" onClick={() => handleCopyInfo(menuRow)} className="w-full rounded px-2 py-1.5 text-left text-xs text-zinc-300 hover:bg-state-mid">
            复制连接信息
          </button>
        </div>,
        document.body,
      )}
    </div>
  );
}
