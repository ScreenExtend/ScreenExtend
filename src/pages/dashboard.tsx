import { Card } from "@/components/ui/card";
import Layout from "@/layout/layout";
import { useState } from "react";
import QRCode from "react-qr-code";
import { Copy } from "lucide-react";
import { Modal } from "flowbite-react";
import { Button } from "@/components/ui/button";

export default function Dashboard() {
  const [value, setValue] = useState("https://dashify.vercel.app/dashboard");

  return (
    <Layout>
      <div className="flex flex-col items-center h-full justify-center">
        <Card className="max-w-96 mx-auto w-full p-1">
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
        </Card>
        <Card className="max-w-96 mx-auto w-full p- mt-4 space-y-3 border-none">
          <div className="flex items-center justify-between border rounded-md">
            <input
              className="w-full p-2 border-none rounded-md bg-transparent"
              type="text"
              value={value}
              onChange={(e) => setValue(e.target.value)}
            />
            <button
              className="p-2 border-l"
              onClick={() => {
                navigator.clipboard.writeText(value);
              }}
            >
              <Copy size={15} />
            </button>
          </div>
          <QrModalComponent value={value} />
        </Card>
      </div>
    </Layout>
  );
}

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
