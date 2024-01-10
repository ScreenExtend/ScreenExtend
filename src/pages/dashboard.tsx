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

export default function Dashboard() {
  return (
    <Layout>
      <div className="flex flex-col items-center h-full justify-center">
        <div className="hidden lg:flex flex-col lg:flex-row items-center justify-center space-y-4 lg:space-y-0 lg:space-x-8">
          <AudioCode />
          <div className="border-r h-[40%]"></div>
          <VideoCode />
        </div>
        <Carousel className="w-full max-w-xs lg:hidden">
          <CarouselContent>
            <CarouselItem>
              <VideoCode />
            </CarouselItem>
            <CarouselItem>
              <AudioCode />
            </CarouselItem>
          </CarouselContent>
          <CarouselPrevious />
          <CarouselNext />
        </Carousel>
      </div>
    </Layout>
  );
}

const AudioCode = () => {
  const [audValue, setAudValue] = useState(
    "https://ScreenExtend.vercel.app/dashboard"
  );

  return (
    <div className="p-1">
      <h2 className="text-2xl font-bold text-center mb-2">Audio</h2>
      <Card className="max-w-96 mx-auto w-full p-1">
        <QRCode
          size={256}
          style={{
            height: "auto",
            maxWidth: "100%",
            width: "100%",
            borderRadius: "0.275rem",
          }}
          value={audValue}
          viewBox={`0 0 256 256`}
        />
      </Card>
      <Card className="max-w-96 mx-auto w-full p- mt-4 space-y-3 border-none">
        <div className="flex items-center justify-between border rounded-md">
          <input
            className="w-full p-2 border-none rounded-md bg-transparent"
            type="text"
            value={audValue}
            onChange={(e) => setAudValue(e.target.value)}
          />
          <button
            className="p-2 border-l"
            onClick={() => {
              navigator.clipboard.writeText(audValue);
            }}
          >
            <Copy size={15} />
          </button>
        </div>
        <QrModalComponent value={audValue} />
      </Card>
    </div>
  );
};

const VideoCode = () => {
  const [vidValue, setVidValue] = useState(
    "https://ScreenExtend.vercel.app/dashboard"
  );
  return (
    <div className="p-1">
      <h2 className="text-2xl font-bold text-center mb-2">Video</h2>
      <Card className="max-w-96 mx-auto w-full p-1">
        <QRCode
          size={256}
          style={{
            height: "auto",
            maxWidth: "100%",
            width: "100%",
            borderRadius: "0.275rem",
          }}
          value={vidValue}
          viewBox={`0 0 256 256`}
        />
      </Card>
      <Card className="max-w-96 mx-auto w-full p- mt-4 space-y-3 border-none">
        <div className="flex items-center justify-between border rounded-md">
          <input
            className="w-full p-2 border-none rounded-md bg-transparent"
            type="text"
            value={vidValue}
            onChange={(e) => setVidValue(e.target.value)}
          />
          <button
            className="p-2 border-l"
            onClick={() => {
              navigator.clipboard.writeText(vidValue);
            }}
          >
            <Copy size={15} />
          </button>
        </div>
        <QrModalComponent value={vidValue} />
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
