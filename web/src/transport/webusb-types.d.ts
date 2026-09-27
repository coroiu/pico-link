// Minimal ambient WebUSB types. TypeScript's bundled `lib.dom.d.ts` does not
// ship the WebUSB API (unlike `@types/w3c-web-usb`, which we deliberately
// don't add as a dependency -- FERN DESIGN's zero/near-zero-dependency
// posture, pico-link-jyhk.8), so this declares only the surface
// `webusb.ts` actually calls. Not a general-purpose WebUSB typing.

interface USBDeviceFilter {
  vendorId?: number;
  productId?: number;
}

interface USBDeviceRequestOptions {
  filters: USBDeviceFilter[];
}

interface USBControlTransferParameters {
  requestType: "standard" | "class" | "vendor";
  recipient: "device" | "interface" | "endpoint" | "other";
  request: number;
  value: number;
  index: number;
}

interface USBInTransferResult {
  data?: DataView;
  status: "ok" | "stall" | "babble";
}

interface USBOutTransferResult {
  bytesWritten: number;
  status: "ok" | "stall" | "babble";
}

interface USBConfiguration {
  configurationValue: number;
}

interface USBDevice {
  readonly vendorId: number;
  readonly productId: number;
  readonly configuration: USBConfiguration | null;
  readonly opened: boolean;
  open(): Promise<void>;
  close(): Promise<void>;
  selectConfiguration(configurationValue: number): Promise<void>;
  claimInterface(interfaceNumber: number): Promise<void>;
  releaseInterface(interfaceNumber: number): Promise<void>;
  controlTransferIn(setup: USBControlTransferParameters, length: number): Promise<USBInTransferResult>;
  controlTransferOut(setup: USBControlTransferParameters, data?: BufferSource): Promise<USBOutTransferResult>;
}

interface USBConnectionEvent extends Event {
  readonly device: USBDevice;
}

interface USB extends EventTarget {
  requestDevice(options: USBDeviceRequestOptions): Promise<USBDevice>;
  getDevices(): Promise<USBDevice[]>;
  addEventListener(type: "connect" | "disconnect", listener: (event: USBConnectionEvent) => void): void;
  removeEventListener(type: "connect" | "disconnect", listener: (event: USBConnectionEvent) => void): void;
}

interface Navigator {
  readonly usb: USB;
}
