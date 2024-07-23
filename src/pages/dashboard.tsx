import { useEffect, useReducer, useState } from "react";
import { Link } from "react-router-dom";

import Layout from "@/layout/layout";
import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Copy } from "lucide-react";
import { Modal } from "flowbite-react";
import QRCode from "react-qr-code";
import {
  Carousel,
  CarouselContent,
  CarouselItem,
  CarouselNext,
  CarouselPrevious,
} from "@/components/ui/carousel";

import { writeText } from "@tauri-apps/api/clipboard";
import { appWindow } from "@tauri-apps/api/window";
import { listen, emit } from "@tauri-apps/api/event";

export default function Dashboard() {
  const [, forceUpdate] = useReducer(x => x + 1, 0);
  const [qrValues, setQrValues] = useState<{ title: string, value: string }[]>([
    {
      title: "Local Hosted Network",
      value: "",
    },
    {
      title: "Same As Current Device",
      value: "d",
    },
    {
      title: "Any Wifi Network",
      value: "d",
    }
  ]);

  useEffect(() => {
    const fetchURLs = async () => {
      await listen("hosted_url", (event) => {
        qrValues[0].value = event.payload as string;
        setQrValues(qrValues);
        forceUpdate();
      });
      await listen("local_url", (event) => {
        qrValues[1].value = event.payload as string;
        setQrValues(qrValues);
        forceUpdate();
      });
      await listen("global_url", (event) => {
        qrValues[2].value = event.payload as string;
        setQrValues(qrValues);
        forceUpdate();
      });
      await emit("dashboard_ready");
    }
    void fetchURLs();
  }, []);

  return (
    <Layout>
      <div className="p-8">
        <h2 className="flex justify-center text-5xl font-semibold">What network is your device connected to?</h2>
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
                Please join a network or start a hosted network in <Link to="/settings" className="underline">settings</Link>.
              </div>
            )
          }
        </div>
        {qrValues.length ? (
          <Carousel className="w-full max-w-xs lg:hidden mx-auto">
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
            Please join a network or create one through <Link to="/settings" className="underline">settings</Link>.
          </div>
        )}
      </div>
    </Layout>
    );
}

const QrDisplay = ({ name, url }: { name: string; url: string }) => {
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
            onClick={async () => await writeText(url)}
          >
            <Copy size={15} />
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
      unlisten = await appWindow.onResized(() => {
        const qrCode = document.getElementById("mainQRCode");
        if (qrCode) {
          qrCode.style.height = (window.innerHeight*0.9 - parseFloat(getComputedStyle(document.documentElement).fontSize)*1.5*2) + "px";
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