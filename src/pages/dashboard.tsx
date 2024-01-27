import { Card } from "@/components/ui/card";
import Layout from "@/layout/layout";
import { useState } from "react";
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
import { cn } from "@/lib/utils";

export default function Dashboard() {
  const [qrValues] = useState([
    {
      title: "Public Domain",
      value: "https://ScreenExtend.vercel.app/dashboard",
    },
    {
      title: "Local Network",
      value: "https://ScreenExtend.vercel.app/dashboard",
    },
    {
      title: "Public Domain",
      value: "https://ScreenExtend.vercel.app/dashboard",
    },
  ]);

  // Uncomment this to use the state and comment the above state

  // const [qrValues] = useState<
  //   {
  //     title: string;
  //     value: string;
  //   }[]
  // >([]);

  return (
    <Layout>
      <div className="p-8">
        <h2 className="text-2xl font-semibold">QR Codes</h2>
      </div>
      <div className="flex flex-col items-center justify-center">
        <div className="hidden lg:flex flex-col lg:flex-row items-center justify-around space-y-4 lg:space-y-0 lg:space-x-8 px-10">
          {qrValues.length ? (
            qrValues.map((qrValue, index) => (
              <>
                <QrDisplay name={qrValue.title} url={qrValue.value} />
                <div
                  className={cn(
                    "border-r h-[120%]",
                    index === qrValues.length - 1 && "hidden"
                  )}
                ></div>
              </>
            ))
          ) : (
            <div className="h-[120%] lg:block text-slate-400">
              Please create join Wifi to continue
            </div>
          )}
        </div>
        {qrValues.length ? (
          <Carousel className="w-full max-w-xs lg:hidden">
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
            Please create join Wifi to continue
          </div>
        )}
      </div>
    </Layout>
  );
}

const QrDisplay = ({ name, url }: { name: string; url: string }) => {
  return (
    <div className="p-1">
      <h2 className="text-2xl font-bold text-center mb-2">{name}</h2>
      <Card className="max-w-96 mx-auto w-full p-1">
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
            onClick={() => {
              navigator.clipboard.writeText(url);
            }}
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

  return (
    <>
      <Button onClick={() => setOpenModal(true)} className="w-full ">
        Expand QR{" "}
      </Button>
      <Modal dismissible show={openModal} onClose={() => setOpenModal(false)}>
        <Modal.Body className="">
          <QRCode
            size={256}
            style={{
              height: "auto",
              maxWidth: "100%",
              width: "100%",
              borderRadius: "0.275rem",
            }}
            value={value}
            viewBox={`0 0 256 256`}
          />
        </Modal.Body>
      </Modal>
    </>
  );
}
