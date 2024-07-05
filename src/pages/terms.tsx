import { useNavigate } from "react-router-dom";
import { ArrowLeft } from "lucide-react";

const Terms = () => {
  const navigate = useNavigate();
  return (
    <div className="py-6 p-20">
      <button
        className="flex items-center gap-2 mb-4 cursor-pointer"
        onClick={() => navigate(-1)}
      >
        <ArrowLeft size={15} />
        Back
      </button>
      <h1 className="text-4xl font-semibold text-center">
        Terms and Conditions
      </h1>
    </div>
  );
};

export default Terms;
