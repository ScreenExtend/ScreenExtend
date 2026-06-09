import { useEffect, useState, useContext } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Copy, Check } from "lucide-react";
import { Modal } from "flowbite-react";
import QRCode from "react-qr-code";
import {
  Carousel,
  CarouselContent,
  CarouselItem,
  CarouselNext,
  CarouselPrevious,
} from "@/components/ui/carousel";

import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { GlobalProviderContext } from "@/components/global-provider";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
const appWindow = getCurrentWebviewWindow();

export default function Dashboard() {
  const { windowQrValues: [qrValues] } = useContext(GlobalProviderContext);

  return (
    <Layout>
      <div className="p-8">
        <h2 className="flex justify-center text-4xl font-semibold">What network is your device connected to?</h2>
      </div>
      <div className="w-full overflow-hidden box-border mb-10">
        <div className="px-8 overflow-auto max-w-full mx-auto box-content hidden lg:flex items-center gap-8">
          {
            qrValues.some(qr => qr.value.length > 0) ? (
              qrValues.map((qrValue) => (
                qrValue.value.length > 0 && (
                  <QrDisplay
                    name={qrValue.title}
                    url={qrValue.value}
                  />
                )
              ))
            ) : (
              <div className="h-[120%] lg:block text-slate-700 dark:text-slate-300 text-lg">
                Join or <b><Link to="/settings" className="underline">Create</Link></b> a Network (none were found)
              </div>
            )
          }
        </div>
        {qrValues.length ? (
          <Carousel className="w-full max-w-xs lg:hidden mx-auto" style={{ msOverflowStyle: "none", scrollbarWidth: "none", overflow: "-moz-scrollbars-none", overflowX: "scroll" }} id={"mainCarousel"}>
            <style>{`
              #mainCarousel::-webkit-scrollbar {
                display: none;
                background: transparent;
                width: 0;
                height: 0;
              }
            `}</style>
            <CarouselContent>
              {qrValues.map((qrValue) => (
                <CarouselItem>
                  <QrDisplay
                    name={qrValue.title}
                    url={qrValue.value}
                  />
                </CarouselItem>
              ))}
            </CarouselContent>
            <CarouselPrevious />
            <CarouselNext />
          </Carousel>
        ) : (
          <div className="text-slate-400 lg:hidden">
            Join or <b><Link to="/settings" className="underline">Create</Link></b> a Network (none were found)
          </div>
        )}
      </div>
    </Layout>
    );
}

const QrDisplay = ({ name, url }: { name: string; url: string }) => {
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    try {
      await writeText(url);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {}
  };

  return (
    <div className="p-1 mx-auto">
      <h2 className="text-2xl font-bold text-center mb-2">{name}</h2>
      <Card className="max-w-96 min-w-72 mx-auto w-full p-1">
        <QRCode
          size={500}
          style={{ height: "auto", maxWidth: "100%", width: "100%", borderRadius: "0.275rem" }}
          value={url}
          viewBox="0 0 256 256"
        />
      </Card>
      <Card className="max-w-96 mx-auto w-full p- mt-4 space-y-3 border-none">
        <div className="flex items-center justify-between border rounded-md">
          <input
            className="w-full p-2 border-none rounded-md bg-transparent"
            type="text"
            value={url}
            disabled
          />
          <button
            className="p-2 border-l"
            onClick={handleCopy}
            aria-label={copied ? "Copied" : "Copy URL"}
          >
            <span className="relative grid place-items-center" style={{ width: 15, height: 15 }}>
              <Copy
                size={15}
                className={`col-start-1 row-start-1 transition-all duration-200 ${copied ? "scale-50 opacity-0" : "scale-100 opacity-100"}`}
              />
              <Check
                size={15}
                className={`col-start-1 row-start-1 text-green-500 transition-all duration-200 ${copied ? "scale-100 opacity-100" : "scale-50 opacity-0"}`}
              />
            </span>
          </button>
        </div>
        <QrModalComponent value={url} />
      </Card>
    </div>
  );
};

function QrModalComponent({ value }: { value: string }) {
  const [openModal, setOpenModal] = useState(false);

  useEffect(() => {
    if (openModal) {
      setTimeout(() => {
        const modalInnerBody = document.getElementsByClassName("max-w-2xl")[0];
        modalInnerBody.removeAttribute("class");
      }, 0);
    }
  }, [openModal]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    const listenToWindowResize = async () => {
      unlisten = await appWindow.onResized(({ payload: size }) => {
        const qrCode = document.getElementById("mainQRCode");
        if (qrCode) {
          qrCode.style.height = (size.height*0.9 - parseFloat(getComputedStyle(document.documentElement).fontSize)*1.5*2) + "px";
        }
      });
    }
    void listenToWindowResize();
    return () => {
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

  return (
    <>
      <Button
        onClick={() => setOpenModal(true)}
        className="w-full"
      >
        Expand QR{" "}
      </Button>
      <Modal
        dismissible
        show={openModal}
        onClose={() => setOpenModal(false)}
        className="bg-black bg-opacity-75"
      >
        <Modal.Body id="mainQRCodeOuter">
          <QRCode
            size={256}
            style={{
              height: window.innerHeight*0.9 - parseFloat(getComputedStyle(document.documentElement).fontSize)*1.5*2,
              maxWidth: "100%",
              width: "100%",
              borderRadius: "0.275rem"
            }}
            value={value}
            viewBox="0 0 256 256"
            id="mainQRCode"
          />
        </Modal.Body>
      </Modal>
    </>
    );
}
