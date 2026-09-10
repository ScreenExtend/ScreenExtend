import React, { createContext, useContext } from "react";
import { type Device } from "@/components/config-provider";

export type AudioOutputsInfo = {
  supported: boolean;
  outputs: { id: string; label: string }[];
  /** The sink the host has asked for, i.e. what is stored against the device. */
  selected: string;
  /**
   * The sink the client is actually playing on, as the client last reported it. It can
   * diverge from `selected`: `setSinkId` needs transient activation in some engines, so a
   * host-side change made while nobody is touching the phone does not apply until the next
   * gesture, and Firefox's own output picker can change it behind the host's back.
   *
   * Optional because it post-dates the payload and older clients will not send it — a
   * missing field means "the client did not say", not "the default".
   */
  current?: string | null;
  /**
   * Whether the ids in `outputs` came back carrying the operating system's own names.
   * `false` means the browser withheld them -- the usual answer without a microphone
   * grant, which this page deliberately no longer spends a prompt on -- so the client
   * named them itself by position ("Output 1", "Default output"). Routing is unaffected:
   * `setSinkId` takes the bare id. The host must simply not present those names as what
   * the hardware is called.
   *
   * Optional, and `null` from a client that predates the field: "the client did not say",
   * which is not the same answer as `false`.
   */
  labelled?: boolean | null;
};

export type QrValue = { title: string; value: string };

export type SessionInfo = {
  sessionId: string;
  otp: string;
  publicSessionsEnabled: boolean;
  hostedNetworkOn: boolean;
};

// The setters are `useState` dispatchers, so this object can be built once and
// never change. A component that only writes never re-renders.
export type GlobalSetters = {
  setHostedNetworkOn: React.Dispatch<React.SetStateAction<boolean>>;
  setOtp: React.Dispatch<React.SetStateAction<string>>;
  setSessionId: React.Dispatch<React.SetStateAction<string>>;
  setQrValues: React.Dispatch<React.SetStateAction<QrValue[]>>;
  setLoaded: React.Dispatch<React.SetStateAction<boolean>>;
  setClosing: React.Dispatch<React.SetStateAction<boolean>>;
  setDevices: React.Dispatch<React.SetStateAction<Device[]>>;
  setPublicSessionsEnabled: React.Dispatch<React.SetStateAction<boolean>>;
  setAvatar: React.Dispatch<React.SetStateAction<string | null>>;
  setZoom: React.Dispatch<React.SetStateAction<number>>;
  setAudioOutputsByIp: React.Dispatch<React.SetStateAction<Record<string, AudioOutputsInfo>>>;
};

const noop = () => {};

export const SettersContext = createContext<GlobalSetters>({
  setHostedNetworkOn: noop,
  setOtp: noop,
  setSessionId: noop,
  setQrValues: noop,
  setLoaded: noop,
  setClosing: noop,
  setDevices: noop,
  setPublicSessionsEnabled: noop,
  setAvatar: noop,
  setZoom: noop,
  setAudioOutputsByIp: noop,
});

export const SessionContext = createContext<SessionInfo>({
  sessionId: "",
  otp: "",
  publicSessionsEnabled: true,
  hostedNetworkOn: false,
});

export const QrValuesContext = createContext<QrValue[]>([]);
export const DevicesContext = createContext<Device[]>([]);
export const AudioOutputsContext = createContext<Record<string, AudioOutputsInfo>>({});
export const AvatarContext = createContext<string | null>(null);
export const ZoomContext = createContext<number>(1);
export const ClosingContext = createContext<boolean>(false);
export const LoadedContext = createContext<boolean>(false);

export const useGlobalSetters = () => useContext(SettersContext);
export const useSession = () => useContext(SessionContext);
export const useQrValues = () => useContext(QrValuesContext);
export const useConnectedDevices = () => useContext(DevicesContext);
export const useAudioOutputs = () => useContext(AudioOutputsContext);
export const useAvatar = () => useContext(AvatarContext);
export const useZoom = () => useContext(ZoomContext);
export const useClosing = () => useContext(ClosingContext);
export const useLoaded = () => useContext(LoadedContext);
