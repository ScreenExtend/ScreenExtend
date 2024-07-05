import { Card } from "@/components/ui/card";
import Layout from "@/layout/layout";
import {useEffect, useState } from "react";
import QRCode from "react-qr-code";
import { Copy } from "lucide-react";
import { Modal } from "flowbite-react";
import { Button } from "@/components/ui/button";
import {
  Carousel,
  CarouselContent,
  CarouselItem,
  CarouselNext,
  CarouselPrevious,
} from "@/components/ui/carousel";
import { writeText } from "@tauri-apps/api/clipboard";
import { listen } from "@tauri-apps/api/event";
// import { Link } from "react-router-dom";

//import { invoke } from "@tauri-apps/api/tauri";
// import { Command } from "@tauri-apps/api/shell";

export default function Dashboard() {
  const [qrValues] = useState<{title: string, value: string}[]>([
    {
      title: "Same As Current Device",
      value: "http://188.112.14.93:5000/",
    },
    {
      title: "Any Other Network",
      value: "https://screenextend.tech/sess/wjduqhsj",
    }
  ]);

  return (
    <Layout>
      <div className="p-8">
        <h2 className="flex justify-center text-5xl font-semibold">What network is your device connected to?</h2>
      </div>
      <div className="w-full overflow-hidden box-border">
        <div className="px-10 overflow-auto max-w-full mx-auto box-content hidden lg:flex items-center gap-8">
          {qrValues.length ? (
            qrValues.map((qrValue) => (
              <QrDisplay name={qrValue.title} url={qrValue.value} />
            ))
              ) : (
                <div className="h-[120%] lg:block text-slate-400">
                  Please join a network or create one via settings.
                </div>
              )}
        </div>
        {qrValues.length ? (
          <Carousel className="w-full max-w-xs lg:hidden mx-auto">
            <CarouselContent>
              {qrValues.map((qrValue) => (
                <CarouselItem>
                  <QrDisplay name={qrValue.title} url={qrValue.value} />
                </CarouselItem>
                ))}
            </CarouselContent>
            <CarouselPrevious />
            <CarouselNext />
          </Carousel>
          ) : (
            <div className="text-slate-400 lg:hidden">
              Please join a network.
              {/*  (one can be created through <Link to={"/settings"} className="underline">settings</Link>) */}
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
          style={{
          height: "auto",
            maxWidth: "100%",
            width: "100%",
            borderRadius: "0.275rem",
          }}
          value={url}
          viewBox={`0 0 256 256`}
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
            onClick={async () => { await writeText(url) }}
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
    async function listenToWindowResize() {
      unlisten = await listen<string>("tauri://resize", () => {
        let qrCode = document.getElementById("mainQRCode");
        if (qrCode) {
          qrCode.style.height = (window.innerHeight*0.9 - parseFloat(getComputedStyle(document.documentElement).fontSize)*1.5*2) + "px";
        }
      });
    }
    listenToWindowResize();
    return () => {
      if (unlisten) {
        unlisten();
      }
    };
  }, []);

  return (
    <>
    <Button onClick={() => setOpenModal(true)} className="w-full ">
      Expand QR{" "}
    </Button>
    <Modal dismissible show={openModal} onClose={() => { setOpenModal(false) }}>
      <Modal.Body id={"mainQRCodeOuter"}>
        <QRCode
          size={256}
          style={{
            height: window.innerHeight*0.9 - parseFloat(getComputedStyle(document.documentElement).fontSize)*1.5*2,
            maxWidth: "100%",
            width: "100%",
            borderRadius: "0.275rem"
          }}
          value={value}
          viewBox={`0 0 256 256`}
          id={"mainQRCode"}
        />
      </Modal.Body>
    </Modal>
    </>
    );
}