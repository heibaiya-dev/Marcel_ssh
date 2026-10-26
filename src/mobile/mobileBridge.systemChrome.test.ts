// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { applyUiTheme, getUiThemePreset } from "@/lib/uiTheme";
import { bindSystemChromeToTheme, setSystemChrome } from "./mobileBridge";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  delete window.AndroidBridge;
  localStorage.clear();
});

const bridge = (fn: unknown) =>
  ({ setSystemChrome: fn }) as unknown as typeof window.AndroidBridge;

describe("setSystemChrome", () => {
  it("浅色主题：把外框底色和 light=true 交给原生（状态栏图标翻成深色）", () => {
    const native = vi.fn();
    window.AndroidBridge = bridge(native);
    setSystemChrome({ background: "#f4f4f5", scheme: "light" });
    expect(native).toHaveBeenCalledWith("#f4f4f5", true);
    setSystemChrome({ background: "#09090b", scheme: "dark" });
    expect(native).toHaveBeenLastCalledWith("#09090b", false);
  });

  it("旧版 APK 没有这个方法 / 非 Android 环境 / 原生抛异常：都不抛", () => {
    expect(() =>
      setSystemChrome({ background: "#fff", scheme: "light" }),
    ).not.toThrow();
    window.AndroidBridge = {} as unknown as typeof window.AndroidBridge;
    expect(() =>
      setSystemChrome({ background: "#fff", scheme: "light" }),
    ).not.toThrow();
    window.AndroidBridge = bridge(() => {
      throw new Error("boom");
    });
    expect(() =>
      setSystemChrome({ background: "#fff", scheme: "light" }),
    ).not.toThrow();
  });
});

describe("bindSystemChromeToTheme", () => {
  it("订阅后主题每次落定都同步给原生，且回放订阅前已应用的主题", () => {
    const native = vi.fn();
    window.AndroidBridge = bridge(native);
    const daylight = getUiThemePreset("daylight")!;
    // 启动早期（initUiThemeEarly）先于订阅应用了缓存主题
    applyUiTheme({ presetId: "daylight", zones: { ...daylight.zones } }, false);
    bindSystemChromeToTheme();
    expect(native).toHaveBeenCalledWith(daylight.zones.bg, true);
    bindSystemChromeToTheme(); // 幂等
    const obsidian = getUiThemePreset("obsidian")!;
    applyUiTheme({ presetId: "obsidian", zones: { ...obsidian.zones } }, false);
    vi.advanceTimersByTime(300);
    expect(native).toHaveBeenLastCalledWith(obsidian.zones.bg, false);
  });

  it("连续变化限流：间隔内只保留最后一个，拖颜色不会每帧都去动系统窗口", () => {
    const native = vi.fn();
    window.AndroidBridge = bridge(native);
    vi.advanceTimersByTime(1000);
    native.mockClear();
    const daylight = getUiThemePreset("daylight")!;
    const obsidian = getUiThemePreset("obsidian")!;
    for (const bg of ["#101010", "#202020", "#303030"]) {
      applyUiTheme(
        { presetId: "custom", zones: { ...obsidian.zones, bg } },
        false,
      );
    }
    applyUiTheme({ presetId: "daylight", zones: { ...daylight.zones } }, false);
    vi.advanceTimersByTime(1000);
    expect(native.mock.calls.length).toBeLessThanOrEqual(2);
    expect(native).toHaveBeenLastCalledWith(daylight.zones.bg, true);
  });
});
