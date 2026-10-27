import { describe, expect, it } from 'vitest';
import {
  WINDOWS_NETWORK_MARKER,
  WINDOWS_PROCESS_MARKER,
  classifyRemotePlatform,
  parsePosixInterfaces,
  parsePosixNetworkOutput,
  parsePosixProcessOutput,
  parseWindowsKillFailure,
  parseWindowsNetworkOutput,
  parseWindowsProcessOutput,
  windowsKillCommand,
  WINDOWS_KILL_FAILED_MARKER,
  WINDOWS_PROCESS_COMMAND,
  WINDOWS_TERMINATED_MARKER,
} from '@/lib/remoteSystem';

describe('remote system output parsers', () => {
  it('parses a Windows process JSON payload after progress noise', () => {
    const output = [
      '#< CLIXML',
      '<Objs />',
      WINDOWS_PROCESS_MARKER,
      JSON.stringify([
        {
          pid: '4321',
          user: '-',
          cpu: 12.5,
          mem: 1.2,
          etime: '01:02:03',
          comm: 'nginx',
          args: '"C:\\nginx.exe" -c conf',
        },
      ]),
    ].join('\n');

    expect(parseWindowsProcessOutput(output)).toEqual([
      {
        pid: '4321',
        user: '-',
        cpu: 12.5,
        mem: 1.2,
        etime: '01:02:03',
        comm: 'nginx',
        args: '"C:\\nginx.exe" -c conf',
      },
    ]);
  });

  it('returns null when the process command did not reach Windows PowerShell', () => {
    expect(parseWindowsProcessOutput("'powershell.exe' is not recognized")).toBeNull();
  });

  it('parses Windows TCP, IPv6 TCP, UDP, and adapter data', () => {
    const output = [
      WINDOWS_NETWORK_MARKER,
      JSON.stringify({
        processes: [
          { pid: 4, name: 'System' },
          { pid: 880, name: 'nginx' },
        ],
        netstat: [
          'Active Connections',
          '  Proto  Local Address          Foreign Address        State           PID',
          '  TCP    0.0.0.0:80             0.0.0.0:0              LISTENING       880',
          '  TCP    [::]:443               [::]:0                 LISTENING       880',
          '  TCP    10.0.0.2:51500         10.0.0.3:443           ESTABLISHED     880',
          '  UDP    0.0.0.0:53             *:*                                    880',
        ],
        interfaces: [
          {
            name: 'Ethernet',
            ip: '10.0.0.2/24',
            mac: '00-11-22-33-44-55',
            state: 'UP',
          },
        ],
      }),
    ].join('\n');

    expect(parseWindowsNetworkOutput(output)).toEqual({
      sockets: [
        {
          proto: 'tcp',
          laddr: '0.0.0.0',
          lport: 80,
          raddr: '0.0.0.0',
          rport: 0,
          state: 'LISTEN',
          pid: 880,
          process: 'nginx',
        },
        {
          proto: 'tcp6',
          laddr: '::',
          lport: 443,
          raddr: '::',
          rport: 0,
          state: 'LISTEN',
          pid: 880,
          process: 'nginx',
        },
        {
          proto: 'tcp',
          laddr: '10.0.0.2',
          lport: 51500,
          raddr: '10.0.0.3',
          rport: 443,
          state: 'ESTABLISHED',
          pid: 880,
          process: 'nginx',
        },
        {
          proto: 'udp',
          laddr: '0.0.0.0',
          lport: 53,
          raddr: '',
          rport: 0,
          state: 'LISTEN',
          pid: 880,
          process: 'nginx',
        },
      ],
      interfaces: [
        {
          name: 'Ethernet',
          ip: '10.0.0.2/24',
          mac: '00-11-22-33-44-55',
          state: 'UP',
        },
      ],
    });
  });

  it('classifies Windows, Linux, macOS and other POSIX uname output', () => {
    expect(classifyRemotePlatform('Linux\n')).toBe('linux');
    expect(classifyRemotePlatform('Darwin\n')).toBe('macos');
    expect(classifyRemotePlatform('FreeBSD\n')).toBe('unix');
    expect(classifyRemotePlatform('MINGW64_NT-10.0\n')).toBe('windows');
    expect(classifyRemotePlatform('unknown\n')).toBeNull();
  });

  it('keeps POSIX process and socket parsing behavior', () => {
    expect(parsePosixProcessOutput(
      '123 root 1.5 0.8 01:02 nginx nginx: worker process\n',
    )).toEqual([
      {
        pid: '123',
        user: 'root',
        cpu: 1.5,
        mem: 0.8,
        etime: '01:02',
        comm: 'nginx',
        args: 'nginx: worker process',
      },
    ]);

    expect(parsePosixNetworkOutput(
      'LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:(("sshd",pid=42,fd=3))\n',
    )).toEqual([
      {
        proto: 'tcp',
        laddr: '0.0.0.0',
        lport: 22,
        raddr: '0.0.0.0',
        rport: 0,
        state: 'LISTEN',
        pid: 42,
        process: 'sshd',
      },
    ]);
  });

  it('parses ip -j interfaces and rejects a non-numeric kill PID', () => {
    expect(parsePosixInterfaces(JSON.stringify([
      {
        ifname: 'eth0',
        addr_info: [{ family: 'inet', local: '10.0.0.2', prefixlen: 24 }],
        address: '00:11:22:33:44:55',
        operstate: 'UP',
      },
    ]))).toEqual([
      {
        name: 'eth0',
        ip: '10.0.0.2/24',
        mac: '00:11:22:33:44:55',
        state: 'UP',
      },
    ]);

    expect(windowsKillCommand('42')).toContain('-EncodedCommand');
    expect(windowsKillCommand('42; Remove-Item C:\\')).toBeNull();
  });
});

describe('Windows PowerShell commands', () => {
  const decode = (command: string) =>
    Buffer.from(command.split('-EncodedCommand ')[1], 'base64').toString('utf16le');

  it('-EncodedCommand 是脚本的 UTF-16LE base64', () => {
    const script = decode(windowsKillCommand('42')!);
    expect(script).toContain('$processId = 42');
    expect(script).not.toContain('__PID__');
  });

  it('命令行长度留在 cmd.exe 的 8191 上限内', () => {
    expect(WINDOWS_PROCESS_COMMAND.length).toBeLessThan(8191);
  });

  it('终止脚本：编码设置失败不能挡住 Stop-Process，失败信息带自家标记', () => {
    const script = decode(windowsKillCommand('42')!);
    expect(script).toMatch(/try \{ \[Console\]::OutputEncoding[^\n]*\} catch \{\}/);
    expect(script).not.toMatch(/\$ErrorActionPreference\s*=\s*'Stop'/);
    expect(script).toContain(WINDOWS_KILL_FAILED_MARKER);
  });

  it('进程运行时长与 ps etime 同口径，不带 TimeSpan 的小数刻度', () => {
    const script = decode(WINDOWS_PROCESS_COMMAND);
    expect(script).not.toContain('StartTime).ToString()');
    expect(script).toContain("'{0}-{1:00}:{2:00}:{3:00}' -f");
  });
});

describe('parseWindowsKillFailure', () => {
  it('成功标记 = 无失败', () => {
    expect(parseWindowsKillFailure(`#< CLIXML
${WINDOWS_TERMINATED_MARKER}
`)).toBeNull();
  });

  it('只取自家标记之后的文本，丢掉 CLIXML 噪声', () => {
    expect(
      parseWindowsKillFailure(
        `#< CLIXML
<Objs />
${WINDOWS_KILL_FAILED_MARKER}拒绝访问。
`,
      ),
    ).toBe('拒绝访问。');
  });

  it('没有任何标记：给固定文案而不是回显原始输出', () => {
    expect(parseWindowsKillFailure('#< CLIXML <Objs>garbage</Objs>')).toBe('未收到远端确认');
    expect(parseWindowsKillFailure(WINDOWS_KILL_FAILED_MARKER)).toBe('远端未说明原因');
  });
});
